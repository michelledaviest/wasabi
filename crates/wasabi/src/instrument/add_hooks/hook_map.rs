use std::collections::HashMap;

use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use wasabi_wasm::{Idx, ValType, ValType::*, Function, Instr, Instr::*, Module, MemoryOp, FunctionType};

use super::block_stack::BlockStackElement;
use super::convert_i64::convert_i64_type;

/*
 * This does 3 things:
 *  - on-demand hook: only hooks for instructions that are actually present in the binary are generated and hooks that were already generated are re-used
 *  - monomorphization of polymorphic hooks: multiple monomorphized hook-variants are generated for one polymorphic instruction, such as call/return/drop/select etc.
 *  - JavaScript and Wasm hook codegen: generate imported functions with some type signature + matching low-level JavaScript functions that are glue-code to the high-level JavaScript hooks the user sees
 */

/// helper struct to encapsulate JavaScript arguments + their Wasm type
pub struct Arg {
    name: String,
    ty: ValType,
}

/// utility
impl Arg {
    /// for the parameter name in the low-level JavaScript function
    fn to_lowlevel_param_name(&self) -> String {
        match self.ty {
            I64 => self.name.clone() + "_low, " + &self.name + "_high",
            _ => self.name.clone(),
        }
    }

    /// for the actual argument when forwarding to the high-level hook
    fn to_lowlevel_long_expr(&self) -> String {
        match self.ty {
            I64 => format!("new Long({})", self.to_lowlevel_param_name()),
            _ => self.name.clone(),
        }
    }
}

/// to make creation of hooks easier and somewhat similar to rust function declarations (i.e. list of "name: type")
macro_rules! args {
    ($($name:ident: $ty:expr),*) => (vec![ $(Arg { name: stringify!($name).into(), ty: $ty }),* ]);
}

pub struct Hook {
    pub idx: Idx<Function>,
    pub wasm: Function,
    pub js: String,
}

impl Hook {
    /// args: do not include the (i32, i32) instruction location, also before i64 -> (i32, i32) lowering
    /// js_args: (quick and dirty, highly unsafe) JavaScript fragment, pasted into the high-level user hook call
    pub fn new(
        lowlevel_name: impl Into<String>,
        args: Vec<Arg>,
        highlevel_name: &str,
        js_args: &str,
    ) -> Self {
        let lowlevel_name = lowlevel_name.into();

        // generate JavaScript low-level hook that is called from Wasm and in turn calls the
        // high-level user analysis hook
        let js = format!("\"{}\": function (func, instr, {}) {{\n    Wasabi.analysis.{}({{func, instr}}, {});\n}},",
                         &lowlevel_name,
                         args.iter().map(Arg::to_lowlevel_param_name).collect::<Vec<_>>().join(", "),
                         highlevel_name,
                         js_args);

        // generate low-level Wasm function to insert into the intrumented module
        let wasm = {
            // prepend two I32 for (function idx, instr idx)
            let mut lowlevel_args = vec![I32, I32];
            lowlevel_args.extend(
                args.iter()
                    // and expand i64 to a tuple of (i32, i32) since there is no JS interop for i64
                    .flat_map(
                        |Arg {
                             name: _name,
                             ref ty,
                         }| convert_i64_type(ty),
                    ),
            );

            Function::new_imported(
                // Hooks do not return anything
                FunctionType::new(&lowlevel_args, &[]),
                "__wasabi_hooks".to_string(),
                lowlevel_name,
                Vec::new()
            )
        };

        Hook {
            wasm,
            js,
            // just a placeholder, replaced on insertion in the map
            idx: Idx::from(0u32),
        }
    }

    pub fn lowlevel_name(&self) -> String {
        self.wasm.import().unwrap().1.to_string()
    }
}

pub struct HookMap {
    /// remember requested (= already inserted) hooks by their low-level name
    /// NOTE wrapped in RwLock to support concurrent lookup (and single-threaded insertion, but this is uncommon anyway)
    map: RwLock<HashMap<String, Hook>>,
    /// needed to determine the function index of the created hooks (should start after the functions
    /// that are already present in the module)
    original_function_count: usize,
}

impl HookMap {
    pub fn new(module: &Module) -> Self {
        HookMap {
            original_function_count: module.functions.len(),
            map: RwLock::new(HashMap::new()),
        }
    }

    /// consumes the internally collected on-demand hooks
    /// returns the to-be-added functions in insertion order (i.e., you can use their idx to
    /// double-check whether no other functions were added to the module in the meantime).
    #[must_use]
    pub fn finish(self) -> Vec<Hook> {
        let mut result: Vec<_> = self.map.into_inner().into_values().collect();
        result.sort_by_key(|hook| hook.idx);
        result
    }

    pub fn instr(&self, instr: &Instr, polymorphic_tys: &[ValType]) -> Instr {
        let name = &mangle_polymorphic_name(instr.to_name(), polymorphic_tys)[..];
        let hook = match *instr {
            /*
                monomorphic instructions:
                - 1 instruction : 1 hook
                - types are determined just from instruction
            */

            Nop | Unreachable => Hook::new(name, args!(), name, ""),

            If(_) => Hook::new(name, args!(condition: I32), "if_", "condition === 1"),
            Br(_) => Hook::new(name, args!(targetLabel: I32, targetInstr: I32), name, "{label: targetLabel, location: {func, instr: targetInstr}}"),
            BrIf(_) => Hook::new(name, args!(condition: I32, targetLabel: I32, targetInstr: I32), name, "{label: targetLabel, location: {func, instr: targetInstr}}, condition === 1"),
            // NOTE js_args is very hacky! We rely on the Hook constructor to close the parenthesis and insert the call statement to endBrTableBlock() here
            BrTable { .. } => Hook::new(name, args!(tableIdx: I32, brTablesInfoIdx: I32), name, "Wasabi.module.info.brTables[brTablesInfoIdx].table, Wasabi.module.info.brTables[brTablesInfoIdx].default, tableIdx); Wasabi.endBrTableBlocks(brTablesInfoIdx, tableIdx, func"),

            MemorySize(_) => Hook::new(name, args!(currentSizePages: I32), name, "currentSizePages"),
            MemoryGrow(_) => Hook::new(name, args!(deltaPages: I32, previousSizePages: I32), name, "deltaPages, previousSizePages"),

            Load(op, _) => {
                let ty = op.to_type().results()[0];
                let args = args!(offset: I32, align: I32, addr: I32, value: ty);
                let instr_name = instr.to_name();
                let js_args = &format!("\"{}\", {{addr, offset, align}}, {}", instr_name, &args[3].to_lowlevel_long_expr());
                Hook::new(name, args, "load", js_args)
            }
            Store(op, _) => {
                let ty = op.to_type().inputs()[1];
                let args = args!(offset: I32, align: I32, addr: I32, value: ty);
                let instr_name = instr.to_name();
                let js_args = &format!("\"{}\", {{addr, offset, align}}, {}", instr_name, &args[3].to_lowlevel_long_expr());
                Hook::new(name, args, "store", js_args)
            }

            Const(val) => {
                let ty = val.to_type();
                let args = args!(value: ty);
                let instr_name = instr.to_name();
                let js_args = &format!("\"{}\", {}", instr_name, args[0].to_lowlevel_long_expr());
                Hook::new(name, args, "const_", js_args)
            }
            Unary(op) => {
                let ty = op.to_type();
                let inputs = ty.inputs().iter().enumerate().map(|(i, &ty)| Arg { name: format!("input{}", i), ty });
                let results = ty.results().iter().enumerate().map(|(i, &ty)| Arg { name: format!("result{}", i), ty });
                let args = inputs.chain(results).collect::<Vec<_>>();
                let instr_name = instr.to_name();
                let js_args = &format!("\"{}\", {}", instr_name, args.iter().map(Arg::to_lowlevel_long_expr).collect::<Vec<_>>().join(", "));
                Hook::new(name, args, "unary", js_args)
            }
            Binary(op) => {
                let ty = op.to_type();
                let inputs = ty.inputs().iter().enumerate().map(|(i, &ty)| Arg { name: format!("input{}", i), ty });
                let results = ty.results().iter().enumerate().map(|(i, &ty)| Arg { name: format!("result{}", i), ty });
                let args = inputs.chain(results).collect::<Vec<_>>();
                let instr_name = instr.to_name();
                let js_args = &format!("\"{}\", {}", instr_name, args.iter().map(Arg::to_lowlevel_long_expr).collect::<Vec<_>>().join(", "));
                Hook::new(name, args, "binary", js_args)
            }


            /*
                polymorphic instructions:
                1. types cannot be determined just from the instruction but must be determined
                   by other means (e.g., stack typing) and are given through polymorphic_tys
                2. no 1:1 relation between instructions and hooks but rather 1:N with mangled names,
                   e.g., 1 polymorphic call instruction -> many monomorphic hooks like call_i32_i64
            */

            Drop => {
                assert_eq!(polymorphic_tys.len(), 1, "drop has only one argument");
                let args = args!(value: polymorphic_tys[0]);
                let js_args = &args[0].to_lowlevel_long_expr();
                Hook::new(name, args, "drop", js_args)
            }
            Select => {
                assert_eq!(polymorphic_tys.len(), 2, "select has two polymorphic arguments");
                assert_eq!(polymorphic_tys[0], polymorphic_tys[1], "select arguments must be equal");
                let args = args!(condition: I32, input0: polymorphic_tys[0], input1: polymorphic_tys[1]);
                let js_args = &format!("condition === 1, {}", args[1..].iter().map(Arg::to_lowlevel_long_expr).collect::<Vec<_>>().join(", "));
                Hook::new(name, args, "select", js_args)
            }
            Local(_, _) => {
                assert_eq!(polymorphic_tys.len(), 1, "local instructions have only one argument");
                let args = args!(index: I32, value: polymorphic_tys[0]);
                let instr_name = instr.to_name();
                let js_args = &format!("\"{}\", {}", instr_name, args.iter().map(Arg::to_lowlevel_long_expr).collect::<Vec<_>>().join(", "));
                Hook::new(name, args, "local", js_args)
            }
            Global(_, _) => {
                assert_eq!(polymorphic_tys.len(), 1, "global instructions have only one argument");
                let args = args!(index: I32, value: polymorphic_tys[0]);
                let instr_name = instr.to_name();
                let js_args = &format!("\"{}\", {}", instr_name, args.iter().map(Arg::to_lowlevel_long_expr).collect::<Vec<_>>().join(", "));
                Hook::new(name, args, "global", js_args)
            }
            Return => {
                let args = polymorphic_tys.iter().enumerate().map(|(i, &ty)| Arg { name: format!("result{}", i), ty }).collect::<Vec<_>>();
                let js_args = &format!("[{}]", args.iter().map(Arg::to_lowlevel_long_expr).collect::<Vec<_>>().join(", "));
                Hook::new(name, args, "return_", js_args)
            }
            Call(_) => {
                let mut args = args!(targetFunc: I32);
                args.extend(polymorphic_tys.iter().enumerate().map(|(i, &ty)| Arg { name: format!("arg{}", i), ty }));
                // NOTE calls the high-level call_pre hook with one argument less than call_indirect, thus tableIdx === undefined since this is a direct call
                let js_args = &format!("targetFunc, [{}]", args[1..].iter().map(Arg::to_lowlevel_long_expr).collect::<Vec<_>>().join(", "));
                Hook::new(name, args, "call_pre", js_args)
            }
            CallIndirect(_, _) => {
                let mut args = args!(tableIndex: I32);
                args.extend(polymorphic_tys.iter().enumerate().map(|(i, &ty)| Arg { name: format!("arg{}", i), ty }));
                let js_args = &format!("Wasabi.resolveTableIdx(tableIndex), [{}], tableIndex", args[1..].iter().map(Arg::to_lowlevel_long_expr).collect::<Vec<_>>().join(", "));
                Hook::new(name, args, "call_pre", js_args)
            }


            /* instructions that need additional information and thus have own method */

            Block(_) | Loop(_) | Else | End => panic!("cannot get hook for block-type instruction with this method, please use the other methods specialized to the block type"),
        };
        self.get_or_insert(hook)
    }

    /* special hooks that do not directly correspond to an instruction or need additional information */

    pub fn start(&self) -> Instr {
        self.get_or_insert(Hook::new("start", vec![], "start", ""))
    }

    pub fn call_post(&self, result_tys: &[ValType]) -> Instr {
        let name = mangle_polymorphic_name("call_post", result_tys);
        let args = result_tys
            .iter()
            .enumerate()
            .map(|(i, &ty)| Arg {
                name: format!("result{}", i),
                ty,
            })
            .collect::<Vec<_>>();
        let js_args = &format!(
            "[{}]",
            args.iter()
                .map(Arg::to_lowlevel_long_expr)
                .collect::<Vec<_>>()
                .join(", ")
        );
        self.get_or_insert(Hook::new(name, args, "call_post", js_args))
    }

    pub fn begin_function(&self) -> Instr {
        self.get_or_insert(Hook::new("begin_function", vec![], "begin", "\"function\""))
    }

    pub fn begin_block(&self) -> Instr {
        self.get_or_insert(Hook::new("begin_block", vec![], "begin", "\"block\""))
    }

    pub fn begin_loop(&self) -> Instr {
        self.get_or_insert(Hook::new("begin_loop", vec![], "begin", "\"loop\""))
    }

    pub fn begin_if(&self) -> Instr {
        self.get_or_insert(Hook::new("begin_if", vec![], "begin", "\"if\""))
    }

    pub fn begin_else(&self) -> Instr {
        self.get_or_insert(Hook::new(
            "begin_else",
            args!(ifInstr: I32),
            "begin",
            "\"else\", {func, instr: ifInstr}",
        ))
    }

    pub fn end(&self, block: &BlockStackElement) -> Instr {
        self.get_or_insert(match *block {
            BlockStackElement::Function { .. } => Hook::new(
                "end_function",
                vec![],
                "end",
                "\"function\", {func, instr: -1}",
            ),
            BlockStackElement::Block { .. } => Hook::new(
                "end_block",
                args!(beginInstr: I32),
                "end",
                "\"block\", {func, instr: beginInstr}",
            ),
            BlockStackElement::Loop { .. } => Hook::new(
                "end_loop",
                args!(beginInstr: I32),
                "end",
                "\"loop\", {func, instr: beginInstr}",
            ),
            BlockStackElement::If { .. } => Hook::new(
                "end_if",
                args!(beginInstr: I32),
                "end",
                "\"if\", {func, instr: beginInstr}",
            ),
            BlockStackElement::Else { .. } => Hook::new(
                "end_else",
                args!(elseInstr: I32, ifInstr: I32),
                "end",
                "\"else\", {func, instr: elseInstr}, {func, instr: ifInstr}",
            ),
        })
    }

    /// returns a Call instruction to the requested hook, which either
    /// A) was freshly generated, since it was not requested with these types before,
    /// B) came from the internal hook map.
    fn get_or_insert(&self, hook: Hook) -> Instr {
        let hook_name = hook.lowlevel_name();
        // This is quite tricky and currently not possible with the std::sync::RwLock:
        // We want to allow parallel reads to the HashMap, but if a hook is not present, we need
        // to insert it, thus requiring a full mutable lock (no parallelism). Always doing exclusive
        // access is however very expensive and writing to the map is not that common anyway (ca.
        // 200 low-level hooks vs. all instructions in the binary, i.e., for large binaries this is
        // very small fraction <1%).
        // Our solution is to aquire a read lock, BUT keep the option open for upgrading it later
        // to a write lock, if the hook could not be found. This is not possible with the standard
        // library. We would either have to
        //   A) get a write lock from the beginning (slow)
        //   B) get a read lock, then get a write lock when the hook was not found (dead lock!)
        //      read lock is still active when waiting for writing :(
        //   C) get a read lock, drop it explicitly, then get a write lock (race condition!)
        //      if some parallel get_or_insert call inserts just between dropping the read lock,
        //      and getting the write lock, we might end up with two hook implementations!
        // Thus: parking_lot::RwLock, which offers an atomic upgrade from read -> write lock
        let map = self.map.upgradable_read();
        let hook_idx = match map.get(&hook_name).map(|h| h.idx) {
            Some(hook_idx) => hook_idx,
            None => {
                let mut map = RwLockUpgradableReadGuard::upgrade(map);
                let idx = (self.original_function_count + map.len()).into();
                map.insert(hook_name, Hook { idx, ..hook });
                idx
            }
        };
        Call(hook_idx)
    }
}

/* utility functions */

/// e.g. "call" + [I32, F64] -> "call_iF"
fn mangle_polymorphic_name(name: &str, tys: &[ValType]) -> String {
    let mut mangled = name.to_string().replace('.', "_");
    if !tys.is_empty() {
        mangled.push('_');
    }
    for ty in tys {
        mangled.push(ty.to_char());
    }
    mangled
}
