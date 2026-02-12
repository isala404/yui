FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive \
    PYTHONUNBUFFERED=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PIP_NO_CACHE_DIR=1 \
    BUN_INSTALL="/root/.bun" \
    IS_SANDBOX=1 \
    PATH="/root/.local/bin:/root/.bun/bin:/opt/pytools/bin:${PATH}"

RUN apt-get update && apt-get install -y --no-install-recommends \
    bash \
    ca-certificates \
    curl \
    fd-find \
    git \
    jq \
    openssh-client \
    python-is-python3 \
    python3 \
    python3-venv \
    ripgrep \
    unzip \
    yt-dlp \
    ffmpeg \
    && ln -sf /usr/bin/fdfind /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

RUN python3 -m venv /opt/pytools \
    && /opt/pytools/bin/pip install --no-compile \
      beautifulsoup4 \
      httpx \
      pydantic \
      python-dateutil \
      pyyaml \
      requests \
    && /opt/pytools/bin/python --version \
    && /opt/pytools/bin/pip --version

RUN curl -fsSL https://bun.sh/install | bash \
    && bun --version

RUN curl -fsSL https://claude.ai/install.sh | bash

RUN bunx skills add https://github.com/vercel-labs/agent-browser --agent claude-code --skill agent-browser --global -y \
    && bunx skills add https://github.com/intellectronica/agent-skills --agent claude-code --skill context7 --global -y

COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

WORKDIR /workspace

ENTRYPOINT ["/entrypoint.sh"]
