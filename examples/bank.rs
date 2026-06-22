//! Watch the checker find a concurrency bug.
//!
//! Two bank accounts hold a fixed total of 100. A `transfer` process moves 10
//! from `a` to `b`; an `audit` process reads both balances and asserts the total
//! is still 100. Neither operation is atomic across the two accounts — there is
//! no lock — so the accounts are read and written at independent scheduling
//! points. That allows two classic bugs:
//!
//! * a **dirty read** — the auditor runs while the transfer is half-done (`a` already debited, `b`
//!   not yet credited), seeing a total of 90;
//! * a **torn read** — the auditor reads `a` before the transfer and `b` after, seeing a total of
//!   110.
//!
//! Optimal DPOR explores the interleavings and reports the first schedule that
//! breaks the invariant. The fix would be to hold a lock across both accounts —
//! a primitive this crate does not have yet, which is rather the point.
//!
//! ```sh
//! cargo run --example bank
//! ```

use interweave::{World, explore};

const TOTAL: i32 = 100;

fn bank(world: &mut World) {
    let a = world.atomic("a", TOTAL);
    let b = world.atomic("b", 0);

    let (from, to) = (a.clone(), b.clone());
    world.spawn("transfer", async move {
        let av = from.load().await;
        from.store(av - 10).await;
        // No lock is held across the two accounts: another process can observe
        // the money "in flight" right here.
        let bv = to.load().await;
        to.store(bv + 10).await;
        Ok(())
    });

    world.spawn("audit", async move {
        let av = a.load().await;
        let bv = b.load().await;
        let total = av + bv;
        if total != TOTAL {
            return Err(
                format!("invariant violated: a={av} + b={bv} = {total}, expected {TOTAL}").into(),
            );
        }
        Ok(())
    });
}

fn main() {
    match explore(&bank, &mut ()) {
        Ok(()) => println!("no interleaving violates the invariant (unexpected for this program)"),
        Err(failed) => {
            println!("found a schedule that breaks the a + b == {TOTAL} invariant:");
            println!("  {failed}");
        }
    }
}
