use std::future::poll_fn;
use std::io::IoSlice;
use std::sync::Mutex;

use env::net::{self, Listener, Tcp};

use crate::device::Device;
use crate::pdu::Exception;

/// Accepts streams on `listener`, one at a time, and answers each request frame on
/// them from `device`, until the listener fails, and gives that error. A frame for
/// another unit than `unit` gets the exception "gateway target failed to respond",
/// as a gateway gives. A stream that ends, fails, or is out of step is dropped, and
/// the next one is accepted. Tests and simulations use it as the far side of a
/// connection.
///
/// # Panics
///
/// When a thread panicked while it held the lock on `device`.
pub async fn serve(
    mut listener: Listener,
    unit: u8,
    device: &Mutex<Device>,
) -> net::Error {
    loop {
        match poll_fn(|cx| listener.poll_accept(cx)).await {
            Ok(stream) => answer(stream, unit, device).await,
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
        reply.extend_from_slice(&frame.header.transaction.to_be_bytes());
        reply.extend_from_slice(&[0, 0, 0, 0, frame.header.unit]);
        if frame.header.unit == unit {
            device
                .lock()
                .expect("no panic under the device lock")
                .answer(frame.pdu, &mut reply);
        } else {
            let function = frame.pdu.first().copied().unwrap_or_default();
            Exception::GatewayTarget.write_to(function, &mut reply);
        }
        let len = frame.len;
        received.drain(..len);
        let length = u16::try_from(reply.len().saturating_sub(6))
            .expect("invariant: a reply PDU is at most 253 bytes");
        if let Some(field) = reply.get_mut(4..6) {
            field.copy_from_slice(&length.to_be_bytes());
        }
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
