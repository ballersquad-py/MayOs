//! Audio and video codecs and containers for MayOS, written from scratch.
//!
//! - Video: H.264 / AVC (Baseline, Main and High profiles, 8-bit 4:2:0)
//! - Audio: AAC-LC, MP3 (MPEG-1/2 Layer III)
//! - Containers: MP4 / MOV / M4A / M4V, Matroska / WebM (MKV), MP3 files
//!
//! Everything is `no_std` + `alloc` and uses integer arithmetic only, so it
//! runs inside the kernel (which has no FPU state) at full speed.

#![no_std]

extern crate alloc;

pub mod aac;
pub mod avi;
pub mod demux;
pub mod mkv;
pub mod mp4;
pub mod pipeline;
pub mod rawaudio;
pub mod resample;
pub mod bits;
pub mod dsp;
pub mod h264;
pub mod mp3;
pub mod tables;
pub mod tags;
pub mod viz;
pub mod vlc;
pub mod yuv;
