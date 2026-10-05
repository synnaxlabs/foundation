"""Writes certificate.txt: the NODE KEY TLS certificate of the node key [1; 32].

It builds the DER from the template in docs/decisions.md and signs it with the
OpenSSL CLI, so it shares no code with `crates/transport`. Run it from this
directory: `python3 certificate.py`.
"""

import subprocess
import tempfile
from pathlib import Path

KEY = bytes([1] * 32)
ED25519 = bytes.fromhex("2b6570")
COMMON_NAME = bytes.fromhex("550403")


def tlv(tag: int, body: bytes) -> bytes:
    n = len(body)
    length = bytes([n]) if n < 0x80 else bytes([0x81, n])
    assert n < 0x100, n
    return bytes([tag]) + length + body


def openssl(*args: str) -> bytes:
    return subprocess.run(["openssl", *args], capture_output=True, check=True).stdout


def main() -> None:
    with tempfile.TemporaryDirectory() as directory:
        key = Path(directory, "key.der")
        key.write_bytes(bytes.fromhex("302e020100300506032b657004220420") + KEY)
        public = openssl("pkey", "-inform", "DER", "-in", str(key), "-pubout",
                         "-outform", "DER")[-32:]
        algorithm = tlv(0x30, tlv(0x06, ED25519))
        name = tlv(0x30, tlv(0x31, tlv(0x30, tlv(0x06, COMMON_NAME)
                                       + tlv(0x0C, b"foundation"))))
        validity = tlv(0x30, tlv(0x17, b"700101000000Z")
                       + tlv(0x18, b"99991231235959Z"))
        spki = tlv(0x30, algorithm + tlv(0x03, b"\x00" + public))
        version = tlv(0xA0, tlv(0x02, b"\x02"))
        serial = tlv(0x02, b"\x01")
        tbs = tlv(0x30, version + serial + algorithm + name + validity + name + spki)
        signed = Path(directory, "tbs.der")
        signed.write_bytes(tbs)
        signature = openssl("pkeyutl", "-sign", "-inkey", str(key), "-keyform", "DER",
                            "-rawin", "-in", str(signed))
        certificate = tlv(0x30, tbs + algorithm + tlv(0x03, b"\x00" + signature))
    text = certificate.hex()
    lines = [text[i:i + 64] for i in range(0, len(text), 64)]
    Path("certificate.txt").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
