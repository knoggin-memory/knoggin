# SPDX-License-Identifier: AGPL-3.0-only
"""Writes a copy of model_q4.onnx with accuracy_level = 4 on every MatMulNBits
node (8-bit kernels; measurement M5) and prints its SHA-256. The weights file
model_q4.onnx_data is not read or changed: the copy refers to it by name, so
put the copy beside it. Fails unless the copy has the SHA-256 that
knoggin-embed pins for the graph.

Usage:
  uv run --no-project --with onnx==1.23.2 python -I \
      scripts/patch_q4_graph.py <model folder>/model_q4.onnx <out>/model_q4.onnx
"""

import hashlib
import sys

import onnx
from onnx import helper

# The published graph at the pinned commit, and the patched graph knoggin-embed pins.
PUBLISHED_SHA256 = "f9eeba97acddf139b8ee2ddf04bc30dceafa88de93fadf74d7644e0d61a477a9"
PATCHED_SHA256 = "1afd951bf447f631786cc1302921f94d4bc30f602ee6a61e8e517d0a3203c8a8"


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit("usage: patch_q4_graph.py <model_q4.onnx> <out>")
    source, out = sys.argv[1], sys.argv[2]
    with open(source, "rb") as f:
        digest = hashlib.sha256(f.read()).hexdigest()
    if digest != PUBLISHED_SHA256:
        sys.exit(f"{source} is not the published graph (SHA-256 {digest})")
    model = onnx.load(source, load_external_data=False)
    patched = 0
    for node in model.graph.node:
        if node.op_type != "MatMulNBits":
            continue
        kept = [a for a in node.attribute if a.name != "accuracy_level"]
        del node.attribute[:]
        node.attribute.extend(kept)
        node.attribute.append(helper.make_attribute("accuracy_level", 4))
        patched += 1
    onnx.save(model, out)
    with open(out, "rb") as f:
        digest = hashlib.sha256(f.read()).hexdigest()
    print(f"patched {patched} MatMulNBits nodes")
    print(f"sha256 {digest}  {out}")
    if digest != PATCHED_SHA256:
        sys.exit(f"{out} is not the pinned patched graph (expected {PATCHED_SHA256})")


if __name__ == "__main__":
    main()
