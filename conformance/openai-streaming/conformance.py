"""Conformance test: the real `openai` Python client against a WireMirage mock.

Not a unit test of WireMirage internals — a black-box check that the actual
SDK (pydantic-validated, real SSE decoder) is happy talking to a mock served
by the host. Run via `run.sh`, which boots the host and registers the routes;
this script only needs the base URL (`WM_BASE`).

What it pins down:
  [1] streaming chunk assembly + finish_reason
  [2] buffered (non-streaming) JSON response
  [3] incremental flush — chunks arrive spread out, proving ADR-0022 streaming
      reaches the client live rather than buffered
  [4] request-body parameterization via the client's own `extra_body=`
  [5] error mapping — an OpenAI-shaped error body surfaces as the right typed
      SDK exception
"""

import os
import sys
import time

import openai
from openai import OpenAI

BASE = os.environ.get("WM_BASE", "http://localhost:8080")
KEY = "sk-mock-not-a-real-key"


def main() -> int:
    client = OpenAI(base_url=f"{BASE}/v1", api_key=KEY, max_retries=0)

    # [1] streaming assembly + finish_reason
    stream = client.chat.completions.create(
        model="gpt-4o-mini",
        messages=[{"role": "user", "content": "Hello mock"}],
        stream=True,
    )
    parts, last = [], None
    for ch in stream:
        last = ch
        if ch.choices[0].delta.content:
            parts.append(ch.choices[0].delta.content)
    text = "".join(parts)
    print(f"[1] stream assembled={text!r} finish_reason={last.choices[0].finish_reason!r}")
    assert text == "You said: Hello mock", text
    assert last.choices[0].finish_reason == "stop"

    # [2] buffered response
    r = client.chat.completions.create(
        model="gpt-4o-mini",
        messages=[{"role": "user", "content": "buffered"}],
    )
    print(f"[2] non-stream content={r.choices[0].message.content!r} object={r.object!r}")
    assert r.choices[0].message.content == "You said: buffered"
    assert r.object == "chat.completion"

    # [3] incremental flush — pace 150ms/token, assert chunks spread over time
    t0, stamps = time.monotonic(), []
    stream = client.chat.completions.create(
        model="gpt-4o-mini",
        messages=[{"role": "user", "content": "one two three four"}],
        stream=True,
        extra_body={"mock_delay_ms": 150},  # [4] forwarded verbatim by the SDK
    )
    for ch in stream:
        if ch.choices[0].delta.content:
            stamps.append(time.monotonic() - t0)
    spread = stamps[-1] - stamps[0] if len(stamps) >= 2 else 0.0
    print(f"[3/4] content_chunks={len(stamps)} arrival_spread={spread:.2f}s (extra_body pacing honored)")
    assert spread > 0.4, f"chunks bunched ({spread:.2f}s) — looks buffered, not streamed"

    # [5] error mapping — OpenAI-shaped 429 -> RateLimitError
    err_client = OpenAI(base_url=f"{BASE}/v1-error", api_key=KEY, max_retries=0)
    try:
        err_client.chat.completions.create(
            model="gpt-4o-mini", messages=[{"role": "user", "content": "x"}]
        )
        raise AssertionError("expected an error, got a response")
    except openai.RateLimitError as e:
        print(f"[5] error mapped -> {type(e).__name__} status={e.status_code} code={e.code!r}")
        assert e.status_code == 429 and e.code == "rate_limit_exceeded"

    print("\nALL CONFORMANCE CHECKS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
