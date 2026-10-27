#!/bin/sh
# Keep words this product must not carry out of files, paths and commit
# messages. The words live as hashes in check-commercial-boundary.py.
set -eu
exec python3 "$(dirname "$0")/check-commercial-boundary.py" "$@"
