#!/usr/bin/env python3
"""Convert the WenQuanYi Micro Hei TrueType collection to standalone woff2 files.

Jellyfin's fallback font endpoint (``GET /FallbackFont/Fonts``) only lists
``.woff``, ``.woff2``, ``.ttf`` and ``.otf`` files, so the ``.ttc`` shipped by
nixpkgs cannot be used as a fallback font as is. woff2 is also the format the
Jellyfin documentation recommends: every listed font is preloaded by the web
client, and the whole folder must stay below 20 MB.

Usage: wqy-microhei-to-woff2.py <wqy-microhei.ttc> <output-directory>
"""

import os
import re
import sys

from fontTools.ttLib import TTCollection


def main() -> None:
    src, outdir = sys.argv[1], sys.argv[2]
    os.makedirs(outdir, exist_ok=True)

    collection = TTCollection(src)
    for index, font in enumerate(collection.fonts):
        family = font["name"].getDebugName(1) or f"face{index}"
        # File names are served in a URL, so keep them free of spaces and
        # other characters that would need escaping.
        name = re.sub(r"[^A-Za-z0-9]+", "-", family).strip("-")
        path = os.path.join(outdir, f"{name}.woff2")
        font.flavor = "woff2"
        font.save(path)
        print(f"{family}: {path} ({os.path.getsize(path)} bytes)")


if __name__ == "__main__":
    main()
