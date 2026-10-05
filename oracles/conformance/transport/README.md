# Transport conformance

The bytes of NODE KEY TLS that every node and SDK must match: the certificate of one
node key, and the suites and groups a dialing node offers, in order. NODE KEY TLS is a
contract, so only the person changes these files.

| File | Holds |
| --- | --- |
| `certificate.txt` | The 219-byte certificate of the node key `[1; 32]` (the Ed25519 private key, 32 bytes of 0x01), in hex. |
| `certificate.py` | The program that writes `certificate.txt`. It builds the DER from the template and signs it with the OpenSSL CLI, so it shares no code with `crates/transport`. |
| `suites.txt` | The TLS 1.3 suites, one IANA code point in hex and its name on each line. |
| `groups.txt` | The key exchange groups, in the same form. |

`crates/transport` reads the files in the tests of `src/tls.rs`:
`certificate::is_the_golden_certificate`,
`handshake::when_a_node_dials_it_offers_the_suites_and_groups_in_order`, and
`handshake::when_an_sdk_offers_one_suite_and_group_the_server_agrees`. These tests are
part of this oracle: to remove one is to weaken the oracle.

```sh
cargo test -p transport --lib tls::tests
python3 certificate.py   # from this directory, needs OpenSSL 3
xxd -r -p certificate.txt | openssl x509 -inform DER -noout -text
```
