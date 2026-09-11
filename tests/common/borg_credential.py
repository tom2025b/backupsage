"""Synthetic credential helper: plumbing/leak checks, no containment proof."""
import json
import os
import sys

# The helper, not BackupSage, reads the synthetic credential. Only paths/config
# appear in argv. Invocation records contain argv only, never the secret.
with open(sys.argv[1], "rb") as credential:
    secret = credential.read()
fd = os.open(sys.argv[2], os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
os.write(fd, (json.dumps(sys.argv) + "\n").encode())
os.close(fd)
# Deliberately hostile diagnostic: the runtime must discard it even on failure.
sys.stderr.buffer.write(secret)
sys.stderr.buffer.flush()
if len(sys.argv) > 3:
    sys.exit(19)
sys.stdout.buffer.write(secret)
