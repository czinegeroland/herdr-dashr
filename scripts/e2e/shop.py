"""A two-service shop instrumented with the real OpenTelemetry SDK.

The end-to-end suite runs it against a dashr session: `orders-api` exports
over OTLP/HTTP (protobuf), `stock-api` over OTLP/gRPC, and the W3C trace
context travels between them in HTTP headers, exactly as in a real
deployment.

    python shop.py stock PORT
    python shop.py orders PORT STOCK_URL
    python shop.py drive ORDERS_URL RUN_ID ITEMS EMAIL [CARD]

Stock fails with "out of stock" for more than 5 items; a card number sent
by the driver is recorded on a span, which a flow can forbid.
"""

import json
import os
import sys
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from opentelemetry import propagate, trace
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor
from opentelemetry.trace import SpanKind, Status, StatusCode


def tracer(service, protocol):
    endpoint = os.environ["OTEL_EXPORTER_OTLP_ENDPOINT"]
    if protocol == "grpc":
        from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter

        exporter = OTLPSpanExporter(endpoint=os.environ["DASHR_OTLP_GRPC"], insecure=True)
    else:
        from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter

        exporter = OTLPSpanExporter(endpoint=endpoint.rstrip("/") + "/v1/traces")
    provider = TracerProvider(resource=Resource.create({"service.name": service, "deployment.environment": "e2e"}))
    provider.add_span_processor(BatchSpanProcessor(exporter, schedule_delay_millis=200))
    trace.set_tracer_provider(provider)
    return trace.get_tracer(service), provider


def call(url, method, body, headers=None):
    headers = dict(headers or {})
    propagate.inject(headers)
    headers["content-type"] = "application/json"
    request = urllib.request.Request(url, data=json.dumps(body).encode(), method=method, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, json.loads(response.read() or b"{}")
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read() or b"{}")


def stock(port):
    tr, _ = tracer("stock-api", "grpc")

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_PUT(self):
            context = propagate.extract({k.lower(): v for k, v in self.headers.items()})
            body = json.loads(self.rfile.read(int(self.headers["content-length"])) or b"{}")
            with tr.start_as_current_span("PUT /stock", context=context, kind=SpanKind.SERVER) as span:
                span.set_attribute("http.request.method", "PUT")
                span.set_attribute("http.route", "/stock")
                with tr.start_as_current_span("reserve stock") as reserve:
                    reserve.set_attribute("order.id", body["order_id"])
                    reserve.set_attribute("stock.requested", body["items"])
                    with tr.start_as_current_span("UPDATE stock", kind=SpanKind.CLIENT) as db:
                        db.set_attribute("db.system", "postgresql")
                        db.set_attribute("db.operation", "UPDATE")
                        time.sleep(0.005)
                    if body["items"] > 5:
                        reserve.set_status(Status(StatusCode.ERROR, f"out of stock for order {body['order_id']}"))
                        span.set_status(Status(StatusCode.ERROR, "409 out of stock"))
                        span.set_attribute("http.response.status_code", 409)
                        self.send_response(409)
                        self.end_headers()
                        self.wfile.write(b'{"error":"out of stock"}')
                        return
                span.set_attribute("http.response.status_code", 200)
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'{"reserved":true}')

    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()


def orders(port, stock_url):
    tr, _ = tracer("orders-api", "http")

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            context = propagate.extract({k.lower(): v for k, v in self.headers.items()})
            body = json.loads(self.rfile.read(int(self.headers["content-length"])) or b"{}")
            with tr.start_as_current_span("POST /orders", context=context, kind=SpanKind.SERVER) as span:
                order_id = f"o-{int(time.time() * 1000) % 100000}"
                span.set_attribute("http.request.method", "POST")
                span.set_attribute("http.route", "/orders")
                span.set_attribute("test.run", self.headers.get("x-test-run", ""))
                span.set_attribute("order.id", order_id)
                span.set_attribute("order.items", body["items"])
                with tr.start_as_current_span("validate order") as validate:
                    validate.set_attribute("customer.email", body["email"])
                    validate.set_attribute("order.items", body["items"])
                    if body.get("card"):
                        validate.set_attribute("card.number", body["card"])
                with tr.start_as_current_span("PUT /stock", kind=SpanKind.CLIENT) as client:
                    client.set_attribute("peer.service", "stock-api")
                    status, _ = call(stock_url + "/stock", "PUT", {"order_id": order_id, "items": body["items"]})
                    client.set_attribute("http.response.status_code", status)
                    if status >= 400:
                        client.set_status(Status(StatusCode.ERROR, f"stock answered {status}"))
                if status >= 400:
                    span.set_status(Status(StatusCode.ERROR, "order rejected"))
                    self.send_response(422)
                    self.end_headers()
                    self.wfile.write(b'{"error":"rejected"}')
                    return
                with tr.start_as_current_span("orders publish", kind=SpanKind.PRODUCER) as publish:
                    publish.set_attribute("messaging.system", "aws_sqs")
                    publish.set_attribute("messaging.destination.name", "order-events")
                    publish.set_attribute("order.id", order_id)
                self.send_response(201)
                self.end_headers()
                self.wfile.write(json.dumps({"order_id": order_id}).encode())

    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()


def drive(orders_url, run_id, items, email, card=None):
    tr, provider = tracer("e2e-driver", "http")
    with tr.start_as_current_span("checkout test", kind=SpanKind.CLIENT) as span:
        span.set_attribute("test.run", run_id)
        span.set_attribute("peer.service", "orders-api")
        body = {"items": int(items), "email": email}
        if card:
            body["card"] = card
        status, answer = call(orders_url + "/orders", "POST", body, {"x-test-run": run_id})
        span.set_attribute("http.response.status_code", status)
    provider.shutdown()
    print(json.dumps({"status": status, "answer": answer, "trace_id": format(span.get_span_context().trace_id, "032x")}))


if __name__ == "__main__":
    mode, args = sys.argv[1], sys.argv[2:]
    if mode == "stock":
        stock(int(args[0]))
    elif mode == "orders":
        orders(int(args[0]), args[1])
    elif mode == "drive":
        drive(*args)
    else:
        sys.exit(f"unknown mode {mode}")
