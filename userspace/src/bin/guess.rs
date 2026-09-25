//! Number guessing game: shows keyboard input reaching a user program.
#![no_std]
#![no_main]

use mstd::{io, print, println, process};

mstd::entry!(main);

fn main() -> i32 {
    // Seed from the clock; a tiny LCG is plenty for a game.
    let mut seed = process::uptime_ms() ^ 0x5deece66d;
    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let secret = (seed >> 33) % 100 + 1;
    println!("I'm thinking of a number between 1 and 100.");
    let mut tries = 0;
    loop {
        print!("Your guess: ");
        let Some(line) = io::read_line() else {
            println!("\nbye!");
            return 0;
        };
        let Ok(n) = line.trim().parse::<u64>() else {
            println!("That's not a number.");
            continue;
        };
        tries += 1;
        if n < secret {
            println!("Higher!");
        } else if n > secret {
            println!("Lower!");
        } else {
            println!("\x1b[92mCorrect!\x1b[0m You got it in {} tries.", tries);
            return 0;
        }
    }
}
