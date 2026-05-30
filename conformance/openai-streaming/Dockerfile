# Conformance client: the real `openai` Python SDK. Run by
# ../run.sh against a wm-host on the host network (--network host),
# with WM_BASE pointing at it. Pinned base image + pinned SDK
# (requirements.txt) so the lane is reproducible — a bump is deliberate,
# which is the whole point of a conformance check.
FROM python:3.11-slim

WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY conformance.py .

ENV WM_BASE=http://localhost:8080
CMD ["python", "conformance.py"]
