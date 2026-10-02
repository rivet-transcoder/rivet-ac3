//! The AC-3 / E-AC-3 encoder (A/52:2018 §8, Annex E): building blocks.

// The frame writer that uses these follows in the next change.
#![allow(dead_code)]

mod bits;
mod exponents;
mod mdct;
mod quant;
mod transient;
