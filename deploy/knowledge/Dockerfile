# TEMPLATE image for the containerised agent-knowledge (see docker-compose.broker.yml). Not built by the app.
# Build:  docker build -f hub/deploy/knowledge/Dockerfile -t agent-knowledge:local hub/knowledge
# Template: adapt the base image and install step to your environment.
FROM python:3.13-slim
# uv from PyPI (pinned) instead of a second image, so the only base image is python:3.13-slim
RUN pip install --no-cache-dir uv==0.11.25
WORKDIR /app
COPY pyproject.toml uv.lock ./
COPY src ./src
RUN uv sync --frozen --no-dev
ENV PATH="/app/.venv/bin:$PATH"
RUN useradd --uid 10001 --no-create-home app && mkdir /data && chown app /data
USER app
EXPOSE 8795
