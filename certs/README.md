# Trust anchors

`roots.pem` is the CA bundle the firmware verifies `api.anthropic.com` against.
It is deliberately tiny: every certificate in it is parsed on each TLS
handshake.

| Root | Why |
|---|---|
| GTS Root R4 | What `api.anthropic.com` chains to today (leaf → WE1 → GTS Root R4). |
| GTS Root R1 | Google Trust Services' RSA root, in case the ECDSA chain is swapped. |
| ISRG Root X1, X2 | Hedge against a move to Let's Encrypt. |

The certificates were exported from the macOS system root store, not downloaded:

```sh
for n in "GTS Root R4" "GTS Root R1" "ISRG Root X1" "ISRG Root X2"; do
  security find-certificate -c "$n" -p /System/Library/Keychains/SystemRootCertificates.keychain
done > certs/roots.pem
```

If the display starts reporting `TLS VERIFICATION FAILED`, check what the API
chains to now and add that root:

```sh
openssl s_client -connect api.anthropic.com:443 -servername api.anthropic.com </dev/null | grep -E "s:|i:"
```

Chain, signature and hostname are verified. Certificate validity *dates* are
not: the device has no trustworthy clock before its first HTTPS response.
