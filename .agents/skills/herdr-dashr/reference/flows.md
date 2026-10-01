# Flows

A flow is the trace a feature should produce. dashr matches every trace
that arrives after the flow was armed against it and keeps the best match
as the verdict.

```json
{
  "name": "checkout",
  "description": "Placing an order reserves stock, charges the card and starts fulfilment",
  "match": {"attributes": {"test.run": "r-42"}},
  "order": "sequence",
  "steps": [
    {"id": "api", "service": "orders-api", "span": "POST /orders", "kind": "server",
     "attributes": {"order.items": 3, "order.id": "re:^o-\\d+$", "test.run": "*"},
     "code": "services/orders/api.py:create_order", "why": "entry point; carries the order id"},
    {"id": "validate", "service": "orders-api", "span": "validate order",
     "attributes": {"customer.email": "re:@example\\.com$", "card.number": "!"},
     "code": "services/orders/validate.py:validate", "why": "no card number may be recorded"},
    {"id": "reserve", "service": "stock-*", "span": "reserve stock",
     "count": {"min": 1, "max": 1}, "max_ms": 500,
     "code": "services/stock/reserve.ts:reserve", "why": "exactly one reservation, no retries"},
    {"id": "charge", "service": "chargecard-fn", "span": "*", "attributes": {"order_state": "ChargeCard"},
     "code": "lambdas/charge/handler.py", "why": "the Step Functions state ran"},
    {"id": "declined", "service": "payments", "span": "charge card", "error": true, "optional": true,
     "why": "the decline path, when the test card is a declined one"}
  ],
  "forbid": [{"service": "*", "span": "retry *"}],
  "no_errors": true,
  "max_ms": 30000,
  "settle_secs": 20
}
```

## Fields

| Field | Meaning |
|---|---|
| `name` | Letters, digits, `-`, `_`, `.`. `dashr flow set` with the same name replaces the flow. |
| `match` | Which traces count: `attributes` some span must carry (a test run id), and/or `root` (a glob on the root span's name). Without it every trace counts. |
| `order` | `sequence` (default): steps start in the order listed. `any`: parallel steps. |
| `steps[].service`, `span` | Globs (`*`, `?`) on the service and span name. |
| `steps[].kind` | `server`, `client`, `producer`, `consumer` or `internal`. |
| `steps[].attributes` | Expected attributes, span or resource: a value (`3` and `"3"` are equal), `"*"` present, `"!"` absent, `"re:<regex>"`. |
| `steps[].error` | The step is expected to fail (an error path under test). Otherwise an error fails it. |
| `steps[].optional` | A missing optional step is fine. |
| `steps[].count` | `{"min": 1, "max": 1}`: how many spans may match — catches retries and duplicates. |
| `steps[].max_ms` | The step's time budget. |
| `steps[].code`, `why` | Where the span is created and what it proves. The human reviews these. |
| `forbid` | Spans that must not appear. |
| `no_errors` | Default `true`: any error span not expected by a step fails the flow. |
| `max_ms` | The whole trace's budget. |
| `settle_secs` | Default 10: quiet seconds before the trace counts as complete. Raise it (20-60) when steps come from pulled sources. |

## Statuses

The verdict is `waiting` (no trace yet), `running` (a trace is arriving),
`pass` or `fail`. An error or a forbidden span fails it at once; a missing
or wrong step fails it once the trace has settled. Each step is `ok`,
`skipped` (optional, absent), `missing`, `out_of_order`, `mismatch`,
`error` or `slow`, with problems such as `order.items: expected 3, got 2`.
Actual values are masked; your expected values are shown as you wrote them.

## Writing good flows

- One step per span you added or rely on; the automatic HTTP/DB spans are
  worth a step only when they prove something (the call happened once).
- Check values that prove behaviour: the amount charged, the state the
  order moved to, the item count — not timestamps or random ids (use
  `"*"` or a regex for those).
- Use `"!"` for data that must never be recorded (card numbers, tokens).
- For an error path, write a separate flow with `"error": true` on the
  failing step and `"no_errors": false` if other spans fail as a result.
- Re-arm (`dashr flow arm`) before each run so an older trace cannot pass.
