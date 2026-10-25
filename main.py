import os
import asyncio
import statistics
import time
import psutil
import httpx
from collections import deque
from contextlib import asynccontextmanager
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel, Field
from typing import Optional

LMSTUDIO = os.getenv("LMSTUDIO_URL", "http://localhost:1234").rstrip("/")
OLLAMA   = os.getenv("OLLAMA_URL",   "http://localhost:11434").rstrip("/")

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

# ── In-memory system profile ──────────────────────────
system_profile = {"name": "My workstation", "notes": ""}

# ── App ───────────────────────────────────────────────
app = FastAPI(title="LLM Monitor")
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
)

# ── Schemas ───────────────────────────────────────────
class TestReq(BaseModel):
    model: str = "local-model"
    prompt: str = "Count from 1 to 20, one number per line."
    max_tokens: int = Field(100, ge=1, le=2048)
    base_url: Optional[str] = None

class StressReq(BaseModel):
    model: str = "local-model"
    prompt: str = "Count from 1 to 20, one number per line."
    max_tokens: int = Field(64, ge=1, le=2048)
    base_url: Optional[str] = None
    concurrency: int = Field(4, ge=1, le=32)
    requests: int = Field(20, ge=1, le=500)

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
    base = (req.base_url or LMSTUDIO).rstrip("/")

    # 1) TTFT – tiny streaming request (1 token)
    ttft_ms = None
    try:
        t0 = time.perf_counter()
        async with httpx.AsyncClient(timeout=60) as c:
            async with c.stream(
                "POST",
                f"{base}/v1/chat/completions",
                json={
                    "model": req.model,
                    "messages": [{"role": "user", "content": req.prompt}],
                    "max_tokens": 1,
                    "stream": True,
                },
            ) as r:
                if r.status_code >= 400:
                    body = await r.aread()
                    return {"error": f"TTFT probe failed: HTTP {r.status_code}: {body.decode(errors='replace')}"}
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
                f"{base}/v1/chat/completions",
                json={
                    "model": req.model,
                    "messages": [{"role": "user", "content": req.prompt}],
                    "max_tokens": req.max_tokens,
                },
            )
    except Exception as e:
        return {"error": f"Inference failed: {e}"}

    elapsed = time.perf_counter() - t0
    if r.status_code >= 400:
        return {"error": f"Inference failed: HTTP {r.status_code}: {r.text}"}
    try:
        body = r.json()
    except Exception as e:
        return {"error": f"Inference failed: invalid JSON: {e}"}
    tok = body.get("usage", {}).get("completion_tokens", 0)
    tps = round(tok / elapsed, 1) if elapsed else 0

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

# ── Models available in LM Studio ─────────────────────
@app.get("/api/models")
async def list_models(base_url: Optional[str] = None):
    base = (base_url or LMSTUDIO).rstrip("/")
    try:
        async with httpx.AsyncClient(timeout=10) as c:
            r = await c.get(f"{base}/v1/models")
        r.raise_for_status()
        data = r.json()
        return [m.get("id") for m in data.get("data", []) if m.get("id")]
    except Exception as e:
        return {"error": str(e)}

import platform

# ── Detailed system profile ───────────────────────────
@app.get("/api/profile")
async def get_profile():
    vm = psutil.virtual_memory()
    prof = {
        "name": system_profile["name"],
        "notes": system_profile["notes"],
        "cpu": platform.processor() or "Unknown CPU",
        "cpu_cores_physical": psutil.cpu_count(logical=False),
        "cpu_cores_logical": psutil.cpu_count(logical=True),
        "ram_total_gb": round(vm.total / 1024**3, 2),
        "os": f"{platform.system()} {platform.release()}",
    }
    if HAS_GPU:
        try:
            prof.update({
                "gpu": pynvml.nvmlDeviceGetName(_GPU).decode() if isinstance(pynvml.nvmlDeviceGetName(_GPU), bytes) else str(pynvml.nvmlDeviceGetName(_GPU)),
                "gpu_vram_gb": round(pynvml.nvmlDeviceGetMemoryInfo(_GPU).total / 1024**3, 2),
                "gpu_driver": pynvml.nvmlSystemGetDriverVersion().decode() if isinstance(pynvml.nvmlSystemGetDriverVersion(), bytes) else str(pynvml.nvmlSystemGetDriverVersion()),
            })
        except Exception as e:
            prof["gpu_error"] = str(e)
    else:
        prof["gpu"] = "None detected"
    return prof

class ProfileReq(BaseModel):
    name: Optional[str] = None
    notes: Optional[str] = None

@app.post("/api/profile")
async def set_profile(req: ProfileReq):
    if req.name is not None:
        system_profile["name"] = req.name
    if req.notes is not None:
        system_profile["notes"] = req.notes
    return system_profile
# ── Stress test ───────────────────────────────────────
@app.post("/api/stress")
async def stress_test(req: StressReq):
    base = (req.base_url or LMSTUDIO).rstrip("/")
    sem = asyncio.Semaphore(req.concurrency)
    latencies = []
    tokens_list = []
    errors = []
    success = 0
    t_start = time.perf_counter()

    async def one():
        nonlocal success
        async with sem:
            t0 = time.perf_counter()
            try:
                async with httpx.AsyncClient(timeout=300) as c:
                    r = await c.post(
                        f"{base}/v1/chat/completions",
                        json={
                            "model": req.model,
                            "messages": [{"role": "user", "content": req.prompt}],
                            "max_tokens": req.max_tokens,
                        },
                    )
                elapsed = time.perf_counter() - t0
                if r.status_code >= 400:
                    errors.append(f"HTTP {r.status_code}")
                    return
                body = r.json()
                tok = body.get("usage", {}).get("completion_tokens", 0)
                latencies.append(elapsed)
                tokens_list.append(tok)
                success += 1
            except Exception as e:
                errors.append(str(e))

    await asyncio.gather(*[one() for _ in range(req.requests)])
    wall = time.perf_counter() - t_start

    def pct(p):
        if not latencies:
            return None
        s = sorted(latencies)
        idx = min(len(s) - 1, int(round(p * (len(s) - 1))))
        return round(s[idx], 3)

    total_tokens = sum(tokens_list)
    result = {
        "model": req.model,
        "base_url": base,
        "concurrency": req.concurrency,
        "requests": req.requests,
        "success": success,
        "errors": len(errors),
        "error_samples": errors[:5],
        "wall_s": round(wall, 2),
        "avg_latency_s": round(sum(latencies) / len(latencies), 3) if latencies else None,
        "p95_latency_s": pct(0.95),
        "total_tokens": total_tokens,
        "agg_tok_per_s": round(total_tokens / wall, 1) if wall else 0,
        "req_per_s": round(success / wall, 2) if wall else 0,
    }
    history.append({
        "timestamp": time.strftime("%H:%M:%S"),
        "model": f"{req.model} (stress x{req.concurrency})",
        "max_tokens": req.max_tokens,
        "ttft_ms": None,
        "total_s": round(wall, 2),
        "tokens": total_tokens,
        "tok_per_s": result["agg_tok_per_s"],
    })
    return result

# ── History ───────────────────────────────────────────
@app.get("/api/history")
async def get_history():
    return list(history)

# ── Serve frontend ────────────────────────────────────
_STATIC = os.path.join(os.path.dirname(os.path.abspath(__file__)), "static")
app.mount("/", StaticFiles(directory=_STATIC, html=True), name="static")

if __name__ == "__main__":
    import uvicorn
    uvicorn.run(app, host="0.0.0.0", port=8501)
