#![feature(attr_literals, specialization, universal_impl_trait, conservative_impl_trait, test)]
#![allow(dead_code, unused_variables, unused_imports)]

extern crate byteorder;
#[macro_use]
extern crate custom_derive;
extern crate rayon;
extern crate test;
extern crate walkdir;

use ast::{highlevel, lowlevel};
use binary::WasmBinary;
use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::path::Path;

mod leb128;
mod ast;
mod binary;
mod instrument;
#[cfg(test)]
mod tests;

/// convenience for working files (which is the most common io::Read/Write anyway).
impl lowlevel::Module {
    pub fn from_file<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Self::decode(&mut BufReader::new(File::open(path)?))
    }

    pub fn to_file<P: AsRef<Path>>(&self, path: P) -> io::Result<usize> {
        self.encode(&mut BufWriter::new(File::create(path)?))
    }
}

impl highlevel::Module {
    pub fn from_file<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Ok(lowlevel::Module::from_file(path)?.into())
    }

    pub fn to_file<P: AsRef<Path>>(self, path: P) -> io::Result<usize> {
        let module: lowlevel::Module = self.into();
        module.to_file(path)
    }
}