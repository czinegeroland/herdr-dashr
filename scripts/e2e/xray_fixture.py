"""Prints what `aws xray batch-get-traces` returns for a Step Functions
execution: a state machine running ten Lambda states and an ECS task.

    python xray_fixture.py TRACE_ID PARENT_SPAN_ID START_EPOCH [FAILING_STATE]

The execution joins an existing trace (TRACE_ID, 32 hex) under the span
that started it (PARENT_SPAN_ID): an event published by a service traced
with OpenTelemetry. The end-to-end suite uses it as a pull source's
command, so the whole path from `dashr source add` to the sequence diagram
runs without an AWS account. Every run prints the same segments, as X-Ray
does when a source re-reads an overlapping window.
"""

import json
import sys

STATES = [
    "ValidateOrder", "ReserveStock", "ChargeCard", "ScoreFraud", "PickWarehouse",
    "BookCourier", "PrintLabel", "NotifyCustomer", "UpdateLedger", "CloseOrder",
]


def main():
    trace, parent = sys.argv[1], sys.argv[2]
    failing = sys.argv[4] if len(sys.argv) > 4 else None
    xray_trace = f"1-{trace[:8]}-{trace[8:]}"
    start = float(sys.argv[3])
    clock = start + 0.01
    state_segments, lambdas = [], []
    for index, name in enumerate(STATES):
        state_id = f"{index + 1:04x}aaaaaaaaaaaa"
        invoke_id = f"{index + 1:04x}bbbbbbbbbbbb"
        function_id = f"{index + 1:04x}cccccccccccc"
        begin, end = clock, clock + 0.08
        state_segments.append({
            "id": state_id, "name": name, "start_time": begin, "end_time": end,
            "subsegments": [{
                "id": invoke_id, "name": "Lambda", "namespace": "aws",
                "start_time": begin + 0.002, "end_time": end - 0.002,
                "aws": {"operation": "Invoke", "function_name": name.lower()},
            }],
        })
        segment = {
            "id": function_id, "name": f"{name.lower()}-fn", "trace_id": xray_trace, "parent_id": invoke_id,
            "start_time": begin + 0.004, "end_time": end - 0.004, "origin": "AWS::Lambda::Function",
            "annotations": {"order_state": name, "test_run": "r-xray"},
        }
        if name == failing:
            segment["fault"] = True
            segment["cause"] = {"exceptions": [{"message": f"{name} failed for customer bob@example.com", "type": "Error"}]}
        lambdas.append(segment)
        clock = end + 0.005
    ecs_state = {
        "id": "00ffaaaaaaaaaaaa", "name": "ShipParcel", "start_time": clock, "end_time": clock + 0.3,
        "subsegments": [{"id": "00ffbbbbbbbbbbbb", "name": "ECS", "namespace": "aws", "start_time": clock + 0.01,
                         "end_time": clock + 0.29, "aws": {"operation": "RunTask", "cluster": "shipping"}}],
    }
    ecs = {
        "id": "00ffcccccccccccc", "name": "shipping-task", "trace_id": xray_trace, "parent_id": "00ffbbbbbbbbbbbb",
        "start_time": clock + 0.02, "end_time": clock + 0.28, "origin": "AWS::ECS::Container",
        "subsegments": [{"id": "00ffdddddddddddd", "name": "DynamoDB", "namespace": "aws", "start_time": clock + 0.05,
                         "end_time": clock + 0.06, "aws": {"operation": "PutItem", "table_name": "shipments"}}],
    }
    machine = {
        "id": "0000eeeeeeeeeeee", "name": "checkout-machine", "trace_id": xray_trace, "parent_id": parent,
        "start_time": start, "end_time": clock + 0.31, "origin": "AWS::StepFunctions::StateMachine",
        "subsegments": state_segments + [ecs_state],
    }
    segments = [machine] + lambdas + [ecs]
    print(json.dumps({
        "Traces": [{"Id": xray_trace, "Duration": clock + 0.31 - start,
                    "Segments": [{"Id": s["id"], "Document": json.dumps(s)} for s in segments]}],
        "UnprocessedTraceIds": [],
    }))


if __name__ == "__main__":
    main()
