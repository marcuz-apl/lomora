import time
import psutil
import httpx
from collections import deque
from contextlib import asynccontextmanager
from fastapi import FastAPI
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel, Field
from typing import Optional

LMSTUDIO = "http://localhost:1234"

# ── GPU (NVIDIA) ──────────────────────────────────────
try:
    import pynvml
    pynvml.nvmlInit()
    _GPU = pynvml.nvmlDeviceGetHandleByIndex(0)
    HAS_GPU = True
except Exception:
    HAS_GPU = False

# ── In-memory history (last 60 runs) ──────────────────
history: deque = deque(maxlen=60)

# ── App ───────────────────────────────────────────────
app = FastAPI(title="LLM Monitor")

# ── Schemas ───────────────────────────────────────────
class TestReq(BaseModel):
    model: str = "local-model"
    prompt: str = "Count from 1 to 20, one number per line."
    max_tokens: int = Field(100, ge=1, le=2048)

# ── Hardware ──────────────────────────────────────────
@app.get("/api/hardware")
async def hardware():
    d = {
        "cpu_percent": psutil.cpu_percent(interval=0.1),
        "ram_percent": psutil.virtual_memory().percent,
        "ram_used_gb": round(psutil.virtual_memory().used / 1024**3, 2),
        "ram_total_gb": round(psutil.virtual_memory().total / 1024**3, 2),
    }
    if HAS_GPU:
        util = pynvml.nvmlDeviceGetUtilizationRates(_GPU)
        mem  = pynvml.nvmlDeviceGetMemoryInfo(_GPU)
        d.update({
            "gpu_util":   util.gpu,
            "gpu_mem_mb": round(mem.used / 1024**2),
            "gpu_mem_tot": round(mem.total / 1024**2),
            "gpu_temp_c": pynvml.nvmlDeviceGetTemperature(_GPU, pynvml.NVML_TEMPERATURE_GPU),
        })
    return d

# ── Run a perf test ───────────────────────────────────
@app.post("/api/test")
async def run_test(req: TestReq):

    # 1) TTFT – tiny streaming request (1 token)
    ttft_ms = None
    try:
        t0 = time.perf_counter()
        async with httpx.AsyncClient(timeout=60) as c:
            async with c.stream(
                "POST",
                f"{LMSTUDIO}/v1/chat/completions",
                json={
                    "model": req.model,
                    "messages": [{"role": "user", "content": req.prompt}],
                    "max_tokens": 1,
                    "stream": True,
                },
            ) as r:
                async for line in r.aiter_lines():
                    if line.startswith("data: ") and "[DONE]" not in line:
                        ttft_ms = (time.perf_counter() - t0) * 1000
                        break
    except Exception as e:
        return {"error": f"TTFT probe failed: {e}"}

    # 2) Token rate – full non-streaming request
    t0 = time.perf_counter()
    try:
        async with httpx.AsyncClient(timeout=300) as c:
            r = await c.post(
                f"{LMSTUDIO}/v1/chat/completions",
                json={
                    "model": req.model,
                    "messages": [{"role": "user", "content": req.prompt}],
                    "max_tokens": req.max_tokens,
                },
            )
    except Exception as e:
        return {"error": f"Inference failed: {e}"}

    elapsed = time.perf_counter() - t0
    body    = r.json()
    tok     = body.get("usage", {}).get("completion_tokens", 0)
    tps     = round(tok / elapsed, 1) if elapsed else 0

    result = {
        "timestamp":   time.strftime("%H:%M:%S"),
        "model":       req.model,
        "max_tokens":  req.max_tokens,
        "ttft_ms":     round(ttft_ms, 1) if ttft_ms else None,
        "total_s":     round(elapsed, 2),
        "tokens":      tok,
        "tok_per_s":   tps,
    }
    history.append(result)
    return result

# ── History ───────────────────────────────────────────
@app.get("/api/history")
async def get_history():
    return list(history)

# ── Serve frontend ────────────────────────────────────
app.mount("/", StaticFiles(directory="static", html=True), name="static")

if __name__ == "__main__":
    import uvicorn
    uvicorn.run(app, host="0.0.0.0", port=8501)
