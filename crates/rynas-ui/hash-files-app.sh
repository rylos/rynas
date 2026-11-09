#!/bin/sh
# Trunk post_build hook: moves the file manager (plain ES modules, not hashed by trunk) to a
# folder named after its contents, and tells the page where it is. rynas-web serves names with a
# content hash as immutable, so opening Files needs no revalidation of its 30 modules, and a
# new version is a new URL: an upgrade cannot mix old and new modules.
set -eu
cd "$TRUNK_STAGING_DIR"
[ -d files-app ] || exit 0
# hash of the file paths and contents (the first path component must not depend on an earlier run)
hash=$(cd files-app && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)
mv files-app "files-app-$hash"
sed -i "s#</head>#<meta name=rynas-files-app content=/files-app-$hash/rynas-files.js></head>#" index.html
