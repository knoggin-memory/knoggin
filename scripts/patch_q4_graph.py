# SPDX-License-Identifier: AGPL-3.0-only
"""Writes a copy of model_q4.onnx with accuracy_level = 4 on every MatMulNBits
node (8-bit kernels; measurement M5) and prints its SHA-256. The weights file
model_q4.onnx_data is not read or changed: the copy refers to it by name, so
put the copy beside it.

Usage:
  uv run --no-project --with onnx==1.23.2 python -I \
      scripts/patch_q4_graph.py <model folder>/model_q4.onnx <out>/model_q4.onnx
"""

import hashlib
import sys

import onnx
from onnx import helper

GRAPH_SHA256 = "f9eeba97acddf139b8ee2ddf04bc30dceafa88de93fadf74d7644e0d61a477a9"


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit("usage: patch_q4_graph.py <model_q4.onnx> <out>")
    source, out = sys.argv[1], sys.argv[2]
    with open(source, "rb") as f:
        digest = hashlib.sha256(f.read()).hexdigest()
    if digest != GRAPH_SHA256:
        sys.exit(f"{source} is not the pinned graph (SHA-256 {digest})")
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
        print(f"patched {patched} MatMulNBits nodes")
        print(f"sha256 {hashlib.sha256(f.read()).hexdigest()}  {out}")


if __name__ == "__main__":
    main()
