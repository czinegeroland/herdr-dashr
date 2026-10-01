# Instrumenting a feature with OpenTelemetry

The goal is a trace that tells the story of the feature: which service
did what, in which order, with which data, and where it failed. Instrument
for the human's review and the flow check, not for coverage.

## What to add

1. **Automatic instrumentation first**, so HTTP, RPC, database, cache and
   queue calls appear without hand-written spans, and context crosses
   services for free.
2. **A span per business step** the feature adds or changes:
   `validate order`, `reserve stock`, `charge card`, `publish order event`.
   Five to fifteen for a feature. A span that proves nothing is noise.
3. **Attributes that prove the step did the right thing**: identifiers
   (`order.id`), counts and amounts (`order.items`, `payment.amount`),
   decisions (`fraud.score`, `stock.available`), outcomes
   (`payment.status`). Namespace them by domain (`order.*`, `payment.*`).
   Use semantic-convention names where they exist (`http.route`,
   `db.system`, `messaging.destination.name`, `peer.service`).
4. **Errors as span status**: on failure, record the exception and set the
   status to error with a short message. A swallowed error that still
   makes the step fail is exactly what end-to-end testing should catch.
5. **A test run id**: copy a header (`x-test-run`) or a field of the input
   into `test.run` on the entry span (and as baggage if it should reach
   every service). Flows select their trace by it.

Never record secrets, tokens, passwords or full card numbers. Personal
data is masked before dashr shows it to you, but the tracing backend keeps
whatever is sent.

## Sending spans

Locally, start each service with the session's environment:

```bash
eval "$(dashr env)"            # sh; `dashr env --shell powershell` on Windows
```

That sets `OTEL_EXPORTER_OTLP_ENDPOINT` (Jaeger's OTLP/HTTP port),
`OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, a fast batch delay, and turns
metrics and logs export off (Jaeger takes traces only). gRPC on 4317 works
too. Services in containers reach the host as `host.docker.internal`; set
`[jaeger] bind = "0.0.0.0"` in dashr's `config.toml` only if the human
agrees (anyone on the network could then send and read spans).

In a deployed environment, keep the service's existing exporter (X-Ray via
ADOT, Azure Monitor, Cloud Trace, the team's collector) and read it back
with a pull source (`reference/sources.md`). Don't redeploy to point
production-like environments at a laptop.

## Per language

**Python**: `pip install opentelemetry-distro opentelemetry-exporter-otlp`,
`opentelemetry-bootstrap -a install`, run with `opentelemetry-instrument
python app.py`. Manual spans:

```python
from opentelemetry import trace
tracer = trace.get_tracer(__name__)
with tracer.start_as_current_span("reserve stock") as span:
    span.set_attribute("order.id", order.id)
    span.set_attribute("stock.requested", qty)
    try:
        reserve(order)
    except OutOfStock as error:
        span.record_exception(error)
        span.set_status(trace.Status(trace.StatusCode.ERROR, "out of stock"))
        raise
```

**Node.js / TypeScript**: `@opentelemetry/sdk-node` +
`@opentelemetry/auto-instrumentations-node` +
`@opentelemetry/exporter-trace-otlp-proto`, loaded with `node --require
./tracing.js` (or `--import` for ESM). Manual spans with
`trace.getTracer('orders').startActiveSpan('reserve stock', span => { ...;
span.end() })`.

**Java / Kotlin**: the OpenTelemetry Java agent
(`-javaagent:opentelemetry-javaagent.jar`) instruments almost everything;
manual spans with `@WithSpan` and `Span.current().setAttribute(...)`.

**.NET**: `OpenTelemetry.Extensions.Hosting` with
`.WithTracing(t => t.AddAspNetCoreInstrumentation().AddHttpClientInstrumentation().AddOtlpExporter())`;
manual spans are `ActivitySource.StartActivity("reserve stock")` with
`activity?.SetTag(...)` and `activity?.SetStatus(ActivityStatusCode.Error, ...)`.

**Go**: `go.opentelemetry.io/otel` with the `otlptracehttp` exporter and
`otelhttp` / `otelgrpc` wrappers; manual spans with `tracer.Start(ctx,
"reserve stock")` — pass `ctx` everywhere, that is the propagation.

**Rust**: `opentelemetry`, `opentelemetry-otlp` and `tracing-opentelemetry`;
`#[tracing::instrument(fields(order.id = %id))]` on business functions.

## Serverless and managed services

- **AWS Lambda**: active tracing (X-Ray) plus the ADOT Lambda layer, or the
  X-Ray SDK. Annotations become attributes in dashr and are searchable in
  X-Ray filter expressions (`annotation.test_run`). Step Functions: enable
  tracing on the state machine; it passes the trace header to Lambda tasks.
- **ECS / EKS**: the ADOT collector as a sidecar or daemon, exporting to
  X-Ray (`awsxray` exporter) or to the team's backend.
- **Azure Functions / App Service**: the Azure Monitor OpenTelemetry distro
  (`azure-monitor-opentelemetry`, `Azure.Monitor.OpenTelemetry.AspNetCore`).
- **Cloud Run / GKE**: the OpenTelemetry SDK with the Cloud Trace exporter,
  or an OTLP collector exporting to Cloud Trace.

## Keeping the trace together

Context must cross every hop, or the trace breaks into pieces (dashr shows
the lost parent as `?`):

- HTTP and gRPC: automatic with instrumented clients and servers.
- Queues and buses (SQS, SNS, Kafka, Service Bus, Pub/Sub): inject the
  context into message attributes on publish and extract it on consume;
  instrumentation libraries usually do. Batches use span links.
- Step Functions → Lambda: automatic with tracing on. → ECS `RunTask`: pass
  the trace header in the container's environment or input.
- Background jobs and cron: start a new trace, link it to the request that
  queued the job.
