# Transport conformance

The bytes of NODE KEY TLS: the certificate of one node key, and the ALPN names,
suites, and groups a dialing node offers, in order. A node matches each file. An SDK
sends no certificate and may offer a subset of the suites and groups. NODE KEY TLS is
a contract, so only the person changes these files.

| File | Holds |
| --- | --- |
| `certificate.txt` | The 219-byte certificate of the node key `[1; 32]` (the Ed25519 private key, 32 bytes of 0x01), in hex. Its public key is `8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c`. |
| `certificate.py` | The program that writes `certificate.txt`. It builds the DER from the template and signs it with the OpenSSL CLI, so it shares no code with `crates/transport`. |
| `alpn.txt` | The ALPN names, one on each line. |
| `suites.txt` | The TLS 1.3 suites, one IANA code point in hex and its name on each line. |
| `groups.txt` | The key exchange groups, in the same form. |

The tests check the code points. The names are for people, as in the IANA registries.

`crates/transport` reads the files in the tests of `src/tls.rs`:
`certificate::matches_the_oracle`,
`handshake::when_a_node_dials_its_client_hello_offers_the_oracle_lists`, and
`handshake::when_an_sdk_offers_one_suite_and_group_the_server_agrees`. These tests are
part of this oracle: to remove one is to weaken the oracle.

```sh
cargo test -p transport --lib tls::tests
python3 certificate.py   # needs OpenSSL 3
xxd -r -p certificate.txt | openssl x509 -inform DER -noout -text
```
