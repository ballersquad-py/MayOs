//! The MayOS web engine: an HTML parser that copes with real-world pages,
//! a CSS parser, style matching with inheritance, and a block/inline
//! layout engine that produces a display list for the browser to paint.
//!
//! The DOM, the CSS parser and selector matching started from robinson
//! (https://github.com/mbrubeck/robinson, MIT licence, (c) 2014 Matt
//! Brubeck) and were extended to tolerate real pages.
#![no_std]

extern crate alloc;

pub mod css;
pub mod dom;
pub mod html;
pub mod layout;
pub mod style;
pub mod url;
