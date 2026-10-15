# Conformance client: the real `openai` Python SDK. Run by
# ../run.sh against a wm-host on the host network (--network host),
# with WM_BASE pointing at it. Pinned base image + pinned SDK
# (requirements.txt) so the lane is reproducible — a bump is deliberate,
# which is the whole point of a conformance check.
# TODO(digest-pin): match the js-engine Dockerfile convention and pin by
# digest so the weekly cron can't go red from base-image drift alone:
#   docker manifest inspect python:3.14-slim   # -> sha256:...
# FROM python:3.14-slim@sha256:<digest>
FROM python:3.14-slim

WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY conformance.py .

ENV WM_BASE=http://localhost:8080
CMD ["python", "conformance.py"]
