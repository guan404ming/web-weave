import pandas as pd
import matplotlib.pyplot as plt
import matplotlib.dates as mdates
from pathlib import Path

OUT = Path(__file__).parent
df = pd.read_csv(OUT / "metrics.csv", parse_dates=["snapshot_at"])

start = df["snapshot_at"].iloc[0]
df["hours"] = (df["snapshot_at"] - start).dt.total_seconds() / 3600

plt.style.use("seaborn-v0_8-whitegrid")
plt.rcParams.update({"figure.dpi": 150, "savefig.bbox": "tight"})

# --- 1. Discovered & Fetched over time ---
fig, ax1 = plt.subplots(figsize=(12, 5))
ax1.plot(df["hours"], df["urls_discovered"] / 1e6, color="#2563eb", linewidth=1.2, label="Discovered")
ax1.set_xlabel("Elapsed (hours)")
ax1.set_ylabel("Discovered URLs (millions)", color="#2563eb")
ax1.tick_params(axis="y", labelcolor="#2563eb")

ax2 = ax1.twinx()
ax2.plot(df["hours"], df["urls_fetched"] / 1e6, color="#dc2626", linewidth=1.2, label="Fetched")
ax2.set_ylabel("Fetched URLs (millions)", color="#dc2626")
ax2.tick_params(axis="y", labelcolor="#dc2626")

fig.suptitle("Discovered & Fetched URLs Over Time", fontsize=14, fontweight="bold")
ax1.set_xlim(0, 48)
fig.savefig(OUT / "1_discovered_fetched.png")
plt.close()

# --- 2. QPS over time ---
fig, ax = plt.subplots(figsize=(12, 4))
ax.plot(df["hours"], df["current_qps"], color="#2563eb", linewidth=0.8, alpha=0.7)
# Rolling average
roll = df["current_qps"].rolling(window=10, min_periods=1).mean()
ax.plot(df["hours"], roll, color="#1d4ed8", linewidth=1.5, label="10-point moving avg")
ax.set_xlabel("Elapsed (hours)")
ax.set_ylabel("QPS")
ax.set_title("QPS Over Time", fontsize=14, fontweight="bold")
ax.set_xlim(0, 48)
ax.legend()
fig.savefig(OUT / "2_qps.png")
plt.close()

# --- 3. Success Rate over time ---
fig, ax = plt.subplots(figsize=(12, 4))
ax.plot(df["hours"], df["success_rate"] * 100, color="#16a34a", linewidth=0.8, alpha=0.7)
roll = (df["success_rate"] * 100).rolling(window=10, min_periods=1).mean()
ax.plot(df["hours"], roll, color="#15803d", linewidth=1.5, label="10-point moving avg")
ax.axhline(y=50, color="#dc2626", linestyle="--", linewidth=0.8, alpha=0.5, label="50% alert threshold")
ax.set_xlabel("Elapsed (hours)")
ax.set_ylabel("Success Rate (%)")
ax.set_title("Success Rate Over Time", fontsize=14, fontweight="bold")
ax.set_xlim(0, 48)
ax.set_ylim(0, 100)
ax.legend()
fig.savefig(OUT / "3_success_rate.png")
plt.close()

# --- 4. P50 / P95 Latency over time ---
fig, ax = plt.subplots(figsize=(12, 4))
ax.fill_between(df["hours"], df["p50_latency_ms"], df["p95_latency_ms"], alpha=0.2, color="#f59e0b", label="P50-P95 range")
ax.plot(df["hours"], df["p50_latency_ms"], color="#f59e0b", linewidth=1, label="P50")
ax.plot(df["hours"], df["p95_latency_ms"], color="#ea580c", linewidth=1, label="P95")
ax.set_xlabel("Elapsed (hours)")
ax.set_ylabel("Latency (ms)")
ax.set_title("P50 / P95 Latency Over Time", fontsize=14, fontweight="bold")
ax.set_xlim(0, 48)
ax.set_ylim(0, 15000)
ax.legend()
fig.savefig(OUT / "4_latency.png")
plt.close()

# --- 5. Discovery Rate per hour (bar chart) ---
df["hour_bucket"] = df["hours"].astype(int)
hourly = df.groupby("hour_bucket")["discovered_last_minute"].sum()
fig, ax = plt.subplots(figsize=(12, 4))
ax.bar(hourly.index, hourly.values / 1e6, color="#8b5cf6", width=0.8, alpha=0.85)
ax.set_xlabel("Elapsed (hours)")
ax.set_ylabel("New URLs Discovered (millions)")
ax.set_title("Discovery Rate Per Hour", fontsize=14, fontweight="bold")
ax.set_xlim(-0.5, 48.5)
fig.savefig(OUT / "5_discovery_rate.png")
plt.close()

print("Done. 5 charts saved to reports/")
