"""Real-binary buffer metric wiring; no real credentials or browser needed."""

import httpx
import pytest
import time

from test_dashboard import dashboard  # noqa: F401 - shared disposable-process fixture


@pytest.mark.parametrize("dashboard", [{"usage_capacity": 7, "alert_capacity": 3}], indirect=True)
def test_configured_buffer_metrics_use_real_capacities_and_existing_auth(dashboard):
    with httpx.Client(base_url=dashboard.base) as client:
        assert client.get("/metrics").status_code == 401
        deadline = time.monotonic() + 5
        while True:
            response = client.get("/metrics", headers=dashboard.headers)
            assert response.status_code == 200
            lines = response.text.splitlines()
            if all(f'sandhi_buffer_{name}{{buffer="{buffer}"}} 0' in lines
                   for buffer in ["usage", "alerts"] for name in ["queued", "in_flight"]):
                break
            assert time.monotonic() < deadline, response.text
            time.sleep(0.01)
        for buffer, capacity in [("usage", 7), ("alerts", 3)]:
            for name, value in [("configured", 1), ("capacity", capacity),
                                ("queued", 0), ("in_flight", 0), ("dropped_total", 0)]:
                assert f'sandhi_buffer_{name}{{buffer="{buffer}"}} {value}' in lines
        samples = [line for line in lines if line.startswith("sandhi_buffer_")]
        assert len(samples) == 10
        assert all('buffer="usage"' in line or 'buffer="alerts"' in line for line in samples)
        assert all("dashboard-test-admin" not in line for line in samples)


def test_mandatory_store_default_buffers_report_real_active_capacities(dashboard):
    response = httpx.get(dashboard.base + "/metrics", headers=dashboard.headers)
    assert response.status_code == 200
    samples = [line for line in response.text.splitlines() if line.startswith("sandhi_buffer_")]
    assert len(samples) == 10
    for buffer, capacity in [("usage", 1024), ("alerts", 256)]:
        assert f'sandhi_buffer_configured{{buffer="{buffer}"}} 1' in samples
        assert f'sandhi_buffer_capacity{{buffer="{buffer}"}} {capacity}' in samples
        for metric in ("queued", "in_flight", "dropped_total"):
            prefix = f'sandhi_buffer_{metric}{{buffer="{buffer}"}} '
            value, = [line.removeprefix(prefix) for line in samples if line.startswith(prefix)]
            assert int(value) >= 0
