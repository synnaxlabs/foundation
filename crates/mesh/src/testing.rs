//! Entry points for the fuzz targets. Needs the `sim` feature.

use crate::change::Change;
use crate::entry;
use crate::message::Message;

/// The bytes that the change record in `bytes` encodes to, or `None` when `bytes` is
/// not a change record.
#[must_use]
pub fn round_trip_change(bytes: &[u8]) -> Option<Vec<u8>> {
    let change = Change::decode(bytes).ok()?;
    let mut out = Vec::new();
    change.encode(&mut out);
    Some(out)
}

/// The bytes that the mesh message in `bytes` encodes to, or `None` when `bytes` is
/// not a mesh message.
#[must_use]
pub fn round_trip_message(bytes: &[u8]) -> Option<Vec<u8>> {
    Message::decode(bytes).map(|message| message.encode())
}

/// The bytes that the `raft` entries in `bytes`, one after another as the log and an
/// append hold them, encode to, or `None` when `bytes` is not such entries.
#[must_use]
pub fn round_trip_entries(mut bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        entry::encode(&entry::decode(&mut bytes)?, &mut out);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_gives_its_bytes_and_other_bytes_give_none() {
        let mut home = vec![1, 0x02, 0x01];
        home.extend([0; 14]);
        home.extend([0x0b, 0x0a]);
        home.extend([0; 14]);
        assert_eq!(round_trip_change(&home), Some(home.clone()));
        assert_eq!(round_trip_change(&home[..32]), None);
        assert_eq!(round_trip_change(&[]), None);
    }

    // The inputs of the fuzz targets `mesh_entries` and `mesh_message`.
    macro_rules! inputs {
        ($target:literal: $($name:literal),+ $(,)?) => {
            [$((
                $name,
                &include_bytes!(concat!(
                    "../../../oracles/fuzz/", $target, "/", $name
                ))[..],
            )),+]
        };
    }

    #[test]
    fn each_entries_input_round_trips_or_is_refused_as_named() {
        for (name, bytes) in inputs!("mesh_entries":
            "no_entries",
            "empty",
            "bytes",
            "voters",
            "three",
        ) {
            assert_eq!(round_trip_entries(bytes).as_deref(), Some(bytes), "{name}");
        }
        for (name, bytes) in inputs!("mesh_entries":
            "bytes_length_past_end",
            "tag_3",
            "voters_cut_in_incoming_count",
            "voters_cut_in_incoming_key",
            "voters_cut_in_outgoing_count",
            "voters_cut_in_outgoing_key",
            "voters_cut_in_grant",
            "voters_cut_in_candidate",
            "voters_cut_in_vote_count",
            "voters_cut_in_vote_key",
            "voters_cut_in_vote_signature",
            "voters_cut_in_second_vote",
            "voters_cut_in_signature",
            "voters_incoming_falling",
        ) {
            assert_eq!(round_trip_entries(bytes), None, "{name}");
        }
    }

    #[test]
    fn each_message_input_round_trips_or_is_refused_as_named() {
        for (name, bytes) in inputs!("mesh_message":
            "raft_pre_vote",
            "raft_pre_vote_reply",
            "raft_vote",
            "raft_vote_reply",
            "raft_heartbeat",
            "raft_heartbeat_reply",
            "raft_append",
            "raft_append_reply",
            "raft_append_reject",
            "raft_proof",
            "raft_chain",
            "propose",
            "proposed",
            "not_leader",
            "not_leader_with_leader",
        ) {
            assert_eq!(round_trip_message(bytes).as_deref(), Some(bytes), "{name}");
        }
        for (name, bytes) in inputs!("mesh_message":
            "none",
            "kind_6",
            "raft_body_10",
            "raft_heartbeat_trailing",
            "raft_chain_cut_in_link",
            "raft_chain_count_no_link",
        ) {
            assert_eq!(round_trip_message(bytes), None, "{name}");
        }
    }
}
