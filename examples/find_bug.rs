//! Watch the checker find a concurrency bug.
//!
//! Two processes each publish their own id into a shared register and then expect
//! to read it back unchanged — the everyday "I just wrote it, so it must still be
//! mine" mistake. Because the store and the load are separate scheduling points,
//! the other process can overwrite the register in between. Exactly one
//! interleaving exhibits this, and Optimal DPOR pins it down.
//!
//! ```sh
//! cargo run --example find_bug
//! ```

use std::error::Error;

use interweave::{Strategy, World, explore};

fn racing_writers(world: &mut World) {
    let reg = world.atomic("reg", 0u32);
    for id in 1..=2u32 {
        let reg = reg.clone();
        world.spawn(format!("writer-{id}"), async move {
            reg.store(id).await;
            // Not atomic with the store above: the other writer can run here.
            let seen = reg.load().await;
            if seen != id {
                return Err(format!("writer-{id} expected {id}, observed {seen}").into());
            }
            Ok(())
        });
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    match explore(&racing_writers, &mut (), Strategy::Optimal) {
        Ok(()) => {
            println!("no interleaving fails (unexpected for this program)");
        }
        Err(failed) => {
            println!("found a failing interleaving:");
            println!("  reason: {failed}");
        }
    }
    Ok(())
}
