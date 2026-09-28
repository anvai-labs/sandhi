# Supply an immutable Python image reference (repository@sha256:...), never a tag.
ARG PYTHON_IMAGE=python@sha256:f77ac9e44ae96ef2c90b8053ea08c31f8be030f824196b0ae4db6d462c84e51f
FROM ${PYTHON_IMAGE}
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1
WORKDIR /opt/evaluator
COPY requirements-container.lock ./requirements-container.lock
RUN python -m pip install --no-cache-dir --require-hashes --only-binary=:all: -r requirements-container.lock
COPY evaluator_app ./app/evaluator_app
COPY __main__.py ./app/__main__.py
RUN python -m zipapp app -o server.pyz && chmod -R a-w /opt/evaluator
USER 10001:10001
ENTRYPOINT ["python", "-I", "-B", "/opt/evaluator/server.pyz", "--http"]
CMD ["/etc/evaluator/service.json"]
