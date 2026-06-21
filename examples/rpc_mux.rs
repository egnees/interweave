//! Watch the checker find a reply-misrouting bug in an RPC connection multiplexer.
//!
//! Several callers share one connection (a single MPSC channel). Each reply frame
//! carries the request id it answers, and a single reader demultiplexes them. The
//! reader is *supposed* to route each reply to its caller — but instead of trusting
//! the id carried in the frame, it consults a shared `in_flight` cell that each
//! caller stamps with its own id just before sending. It looks airtight: every frame
//! is tagged, the reader checks the tag, and the FIFO channel preserves order. The
//! trap is that `in_flight` is shared mutable state updated out of band — a second
//! caller can stamp its id over the first's before the reader has correlated the
//! first caller's reply, so the reader hands one caller's result to another call.
//! Correlation state must travel *with* the message, not live in a shared slot.
//!
//! `recv` blocks on an empty connection, so the state space is finite: the reader
//! demuxes exactly as many frames as were sent.
//!
//! ```sh
//! cargo run --example rpc_mux
//! ```

use interweave::{Strategy, World, explore};

// A reply frame on the shared connection: the request id it answers and the result
// destined for that request's caller (call k's result is k * 10).
#[derive(Debug, Clone, Copy)]
struct Reply {
    id: i32,
    result: i32,
}

fn rpc_mux(world: &mut World) {
    // The id of the call currently believed to be awaiting a reply on the connection.
    let in_flight = world.atomic("in_flight", -1);
    let (conn, reader) = world.channel::<Reply>("conn");

    // Two callers share the one connection. Each marks its call in-flight, then sends
    // its request; the server echoes a reply frame back tagged with the same id.
    for id in 0..2 {
        let (in_flight, conn) = (in_flight.clone(), conn.clone());
        world.spawn(format!("caller-{id}"), async move {
            in_flight.store(id).await;
            conn.send(Reply {
                id,
                result: id * 10,
            })
            .await;
            Ok(())
        });
    }

    // The reader demultiplexes replies, handing each frame's result to whatever call
    // it thinks is in flight — instead of to the id carried in the frame.
    world.spawn("reader", async move {
        for _ in 0..2 {
            let frame = reader.recv().await;
            let routed_to = in_flight.load().await;
            // The result we deliver must belong to the call we deliver it to.
            if frame.result != routed_to * 10 {
                return Err(format!(
                    "call {routed_to} received call {}'s result ({})",
                    frame.id, frame.result
                )
                .into());
            }
        }
        Ok(())
    });
}

fn main() {
    match explore(&rpc_mux, &mut (), Strategy::Optimal) {
        Ok(()) => println!("every reply reaches its caller (unexpected for this program)"),
        Err(failed) => {
            println!("found a schedule where a reply is misrouted:");
            println!("  {failed}");
            println!("  {failed:?}");
        }
    }
}
