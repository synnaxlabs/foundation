use std::future::poll_fn;
use std::io::IoSlice;
use std::pin::pin;
use std::sync::Mutex;

use env::net::{self, Listener, Tcp};

use crate::device::Device;
use crate::pdu::Exception;

/// Accepts streams on `listener` and answers each request frame on the newest from
/// `device`, until the listener fails, and gives that error. A new stream drops the
/// one before it, as a device with one connection does, so that a client that lost
/// its stream with no close gets an answer when it comes back. A frame for another
/// unit than `unit` gets the exception "gateway target failed to respond", as a
/// gateway gives. A stream that fails or is out of step is dropped. Tests and
/// simulations use it as the far side of a connection.
///
/// # Panics
///
/// When a thread panicked while it held the lock on `device`.
pub async fn serve(
    mut listener: Listener,
    unit: u8,
    device: &Mutex<Device>,
) -> net::Error {
    let first = match poll_fn(|cx| listener.poll_accept(cx)).await {
        Ok(stream) => stream,
        Err(error) => return error,
    };
    let mut answering = pin!(answer(first, unit, device));
    let mut ended = false;
    loop {
        let accepted = poll_fn(|cx| {
            if !ended {
                ended = answering.as_mut().poll(cx).is_ready();
            }
            listener.poll_accept(cx)
        })
        .await;
        match accepted {
            Ok(stream) => {
                answering.set(answer(stream, unit, device));
                ended = false;
            }
            Err(error) => return error,
        }
    }
}

/// Answers each request frame on `stream` until it ends, fails, or is out of step.
async fn answer(mut stream: Tcp, unit: u8, device: &Mutex<Device>) {
    let (mut received, mut reply) = (Vec::new(), Vec::new());
    let mut buffer = [0; 260];
    loop {
        let frame = match super::decode(&received) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                match poll_fn(|cx| stream.poll_read(cx, &mut buffer)).await {
                    Ok(0) => {
                        // Errors leave nothing to do: the stream ends either way.
                        drop(poll_fn(|cx| stream.poll_close(cx)).await);
                        return;
                    }
                    Ok(n) => received.extend(buffer.iter().take(n)),
                    Err(_) => return,
                }
                continue;
            }
            Err(_) => return,
        };
        reply.clear();
        let start = super::open(frame.header, &mut reply);
        if frame.header.unit == unit {
            device
                .lock()
                .expect("no panic under the device lock")
                .answer(frame.pdu, &mut reply);
        } else {
            let function = *frame.pdu.first().expect("invariant: a PDU is not empty");
            Exception::GatewayTarget.write_to(function, &mut reply);
        }
        super::seal(&mut reply, start);
        let len = frame.len;
        received.drain(..len);
        if write(&mut stream, &reply).await.is_err() {
            return;
        }
    }
}

/// Sends all of `bytes`.
async fn write(stream: &mut Tcp, bytes: &[u8]) -> Result<(), net::Error> {
    let mut rest = bytes;
    while !rest.is_empty() {
        let parts = [IoSlice::new(rest)];
        let n = poll_fn(|cx| stream.poll_write(cx, &parts)).await?;
        rest = rest.get(n..).unwrap_or_default();
    }
    Ok(())
}
