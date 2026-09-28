"""Generate a bounded-size, deliberately slow ONNX graph for cancellation tests."""

import argparse
import json
from pathlib import Path

import numpy as np
from onnx import TensorProto, checker, helper, numpy_helper


def build(width):
    body = helper.make_graph(
        [
            helper.make_node("Identity", ["condition"], ["next_condition"]),
            helper.make_node("Add", ["state", "step"], ["next_state"]),
        ],
        "slow_body",
        [
            helper.make_tensor_value_info("iteration", TensorProto.INT64, []),
            helper.make_tensor_value_info("condition", TensorProto.BOOL, []),
            helper.make_tensor_value_info("state", TensorProto.FLOAT, [1]),
        ],
        [
            helper.make_tensor_value_info("next_condition", TensorProto.BOOL, []),
            helper.make_tensor_value_info("next_state", TensorProto.FLOAT, [1]),
        ],
        [numpy_helper.from_array(np.asarray([1e-20], dtype=np.float32), "step")],
    )
    graph = helper.make_graph(
        [
            helper.make_node("ReduceSum", ["counts"], ["total"], keepdims=0),
            helper.make_node("Mul", ["total", "scale"], ["scaled"]),
            helper.make_node("Cast", ["scaled"], ["iterations"], to=TensorProto.INT64),
            helper.make_node(
                "Loop", ["iterations", "true", "initial"], ["raw_score"], body=body
            ),
            helper.make_node("Clip", ["raw_score", "zero", "one"], ["score"]),
        ],
        "cancellation_fixture",
        [helper.make_tensor_value_info("counts", TensorProto.FLOAT, [2, width])],
        [helper.make_tensor_value_info("score", TensorProto.FLOAT, [1])],
        [
            numpy_helper.from_array(np.asarray(100000000, dtype=np.float32), "scale"),
            numpy_helper.from_array(np.asarray(True), "true"),
            numpy_helper.from_array(np.asarray([0], dtype=np.float32), "initial"),
            numpy_helper.from_array(np.asarray(0, dtype=np.float32), "zero"),
            numpy_helper.from_array(np.asarray(1, dtype=np.float32), "one"),
        ],
    )
    model = helper.make_model(
        graph, opset_imports=[helper.make_opsetid("", 13)], ir_version=8
    )
    checker.check_model(model, full_check=True)
    return model.SerializeToString()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("export_directory", type=Path)
    args = parser.parse_args()
    descriptor = json.loads((args.export_directory / "preprocessing.json").read_text())
    with (args.export_directory / "slow.onnx").open("xb") as output:
        output.write(build(len(descriptor["vocabulary"])))
