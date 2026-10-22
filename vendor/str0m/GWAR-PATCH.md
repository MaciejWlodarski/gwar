# str0m 0.24.1: minimum payload for resend padding

`StreamTx::poll_packet_padding` uses blank padding instead of a cached packet
smaller than `MAX_BLANK_PADDING_PAYLOAD_SIZE` (240 bytes). NACK repair is
unchanged. Reusing tiny Text deltas amplified payload-sized probe clusters
with RTP/SRTP headers and exhausted the SFU's wire-byte reserve.

The Gwar screen simulation covers 16-byte deltas at 5 fps and the e2e screen
tests use 6-byte packets again. Remove this patch when upstream provides the
same protection. No other source changes.
