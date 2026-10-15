# Conformance client: the real `openai` Python SDK. Run by
# ../run.sh against a wm-host on the host network (--network host),
# with WM_BASE pointing at it. Pinned base image + pinned SDK
# (requirements.txt) so the lane is reproducible — a bump is deliberate,
# which is the whole point of a conformance check.
FROM python:3.14-slim@sha256:a2b82f3c48559aa0a8446d9af49826b6e2b2016f4cd2afabfe6013ec53729170

WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY conformance.py .

ENV WM_BASE=http://localhost:8080
CMD ["python", "conformance.py"]
