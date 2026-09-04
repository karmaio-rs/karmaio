# TLS test identity

These DER files are test and examples only credentials for a private CA and a localhost server.
They are public repository fixtures and must never be used outside the test suite.

- `ca.der`: self-signed `karmaio-test-ca` trust anchor.
- `localhost.der`: server certificate signed by that CA, with SANs for
  `localhost` and `127.0.0.1` and the `serverAuth` extended key usage.
- `localhost-key.der`: unencrypted PKCS#8 P-256 private key for the localhost certificate.

Both certificates are valid from 2026-09-04 through 2036-09-01. Their SHA-256 fingerprints are:

- CA: `9C:A0:20:2C:8A:8A:6B:5C:4B:60:2D:42:56:3B:8B:23:90:CD:25:B0:F7:93:32:58:F0:4B:F7:EE:33:EF:AD:5C`
- localhost: `05:5E:88:0B:46:00:5E:22:00:84:78:84:94:34:02:0A:1A:3D:5F:FD:22:09:7E:A2:EC:63:65:F8:B2:E3:8F:89`

They were generated with OpenSSL 3 using a P-256 CA key, a P-256 localhost key and CSR, `openssl x509 -req -copy_extensions copy`,
`openssl x509 -outform DER`, and `openssl pkcs8 -topk8 -nocrypt -outform DER`.
Regeneration must preserve the SAN, key-usage, extended-key-usage, and CA basic-constraints extensions; update the dates and fingerprints above when replacing them.
