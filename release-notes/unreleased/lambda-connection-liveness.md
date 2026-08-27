# Detect stalled connections to the AWS Lambda API

## Behavioral Change

### What Changed

Restate now uses its own HTTP client for calls to the AWS Lambda API instead of the one that ships
with the AWS SDK, so that HTTP/2 keep-alive pings can be enabled on those connections. The existing
`worker.invoker.http2-keep-alive-interval` and `http2-keep-alive-timeout` options now govern
connections to the Lambda API as well as connections to HTTP deployments. There are no new options,
and the defaults are unchanged at `40s` and `20s`.

Only the Lambda transport is affected. The credential providers and STS continue to use the SDK's
own HTTP client.

### Why This Matters

Lambda invocations are multiplexed onto a small number of long-lived HTTP/2 connections. If one of
those connections stops making progress — for example because the network silently drops the flow —
the AWS SDK's client had no way to notice: every invocation on that connection would hang until the
kernel exhausted its TCP retransmission budget, around 15 minutes later. With keep-alive pings the
same failure is detected within the ping interval plus its timeout, the affected invocations fail,
and the invoker retries them on a healthy connection.

### Impact on Users

- Existing deployments pick up the behaviour on upgrade; no configuration is required.
- Invocations against Lambda deployments that would previously have hung for the duration of a
  network blackhole now fail within about a minute and are retried, so the recovery is visible as
  invoker retries rather than as stuck invocations.
- Setting `worker.invoker.http2-keep-alive-interval` to `0` disables pings, which restores the old
  behaviour for both HTTP deployments and Lambda.

### Migration Guidance

No action required.
