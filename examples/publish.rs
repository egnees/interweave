//! Watch the checker find an unsafe-publication bug.
//!
//! A `producer` hands a value to a `consumer` through a one-shot `ready` flag,
//! but it raises the flag *before* it writes the value. A consumer that trusts
//! the flag — reads the value once it sees `ready == 1` — can therefore observe
//! the flag set while the value is still its old, uninitialized contents. This is
//! the classic "publish a reference before the object is built" race behind
//! broken double-checked locking.
//!
//! There is no loop here: the consumer checks the flag once. The model checker
//! does the waiting for us by exploring the schedule where the flag is already
//! raised — so this stays a pure safety check, with finitely many interleavings.
//! Writing the value *before* raising the flag would fix it; the checker would
//! then explore every interleaving without tripping the assertion.
//!
//! ```sh
//! cargo run --example publish
//! ```

use interweave::{Strategy, World, explore};

const VALUE: i32 = 42;
const READY: i32 = 1;

fn publish(world: &mut World) {
    let data = world.atomic("data", 0);
    let ready = world.atomic("ready", 0);

    let (data_w, ready_w) = (data.clone(), ready.clone());
    world.spawn("producer", async move {
        ready_w.store(READY).await; // announce the value...
        data_w.store(VALUE).await; // ...before it has been written
        Ok(())
    });

    world.spawn("consumer", async move {
        if ready.load().await == READY {
            let v = data.load().await;
            if v != VALUE {
                return Err(format!("read the value before it was published: {v}").into());
            }
        }
        Ok(())
    });
}

fn main() {
    match explore(&publish, &mut (), Strategy::Optimal) {
        Ok(()) => println!("no interleaving reads stale data (unexpected for this program)"),
        Err(failed) => {
            println!("found a schedule that reads the value before it was published:");
            println!("  {failed}");
        }
    }
}
