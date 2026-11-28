# Conformance client: the real `openai` Python SDK. Run by
# ../run.sh against a wm-host on the host network (--network host),
# with WM_BASE pointing at it. Pinned base image + pinned SDK
# (requirements.txt) so the lane is reproducible — a bump is deliberate,
# which is the whole point of a conformance check.
FROM python:3.15-slim@sha256:65a65fc7b30b06cb6df4930fd4650697cb23f4c2698ab81fc87e4b031bc36928

WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY conformance.py .

ENV WM_BASE=http://localhost:8080
CMD ["python", "conformance.py"]
