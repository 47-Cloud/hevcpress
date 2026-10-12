# Security policy

hevcpress reads files from untrusted sources, so bugs in the parser matter. Of
particular interest:

- input that makes `compress` or `decompress` crash, hang or use unbounded memory;
- a `.hvp` file that decompresses to something other than the original without
  a checksum error;
- anything that lets a crafted file write outside the requested output path.

Please report these privately using GitHub's private vulnerability reporting:
the Security tab, then "Report a vulnerability". You should get a reply within
a week. Please don't open a public issue for a security problem.

Only the latest release is supported.
