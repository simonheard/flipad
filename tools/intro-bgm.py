# Flip Pad intro BGM: 120 BPM, C major, 31 bars (62 s). Scene cuts fall on bar lines.
# Usage: python3 tools/intro-bgm.py out.wav  (needs numpy and ffmpeg)
# Then: ffmpeg -i out.wav -af loudnorm=I=-16:TP=-1.5 flipad-bgm.wav
import numpy as np, sys
SR = 48000
BEAT = 0.5
BAR = 2.0
DUR = 62.0
N = int(SR * DUR)
L = np.zeros(N); R = np.zeros(N)
rng = np.random.default_rng(7)

def hz(m): return 440.0 * 2 ** ((m - 69) / 12)

def add(buf, start, sig, gain=1.0):
    i = int(start * SR)
    if i >= N: return
    sig = sig[: N - i]
    buf[i:i + len(sig)] += sig * gain

def addst(start, sig, gain=1.0, pan=0.0):
    add(L, start, sig, gain * np.sqrt(0.5 * (1 - pan)))
    add(R, start, sig, gain * np.sqrt(0.5 * (1 + pan)))

def saw(f, t, harm=40, bright=1.0):
    out = np.zeros_like(t)
    kmax = int(min(harm, (SR / 2.2) / f))
    for k in range(1, kmax + 1):
        out += np.sin(2 * np.pi * k * f * t) / k * np.exp(-(k - 1) / (harm * bright))
    return out * 0.6

def env_adsr(n, a, d, s, r, hold):
    t = np.arange(n) / SR
    e = np.where(t < a, t / max(a, 1e-4), s + (1 - s) * np.exp(-(t - a) / max(d, 1e-4)))
    e = np.where(t > hold, e * np.exp(-(t - hold) / max(r, 1e-4)), e)
    return e

# ---- song structure (bar -> section) ----
def section(b):
    if b < 3: return "intro"
    if b < 8: return "groove"
    if b < 17: return "full"
    if b < 21: return "break"
    if b < 28: return "full"
    return "outro"

CHORDS = [  # (bass, pad voicing)
    (36, [60, 64, 67, 71]),  # Cmaj7
    (43, [59, 62, 67, 74]),  # G
    (45, [60, 64, 69, 72]),  # Am
    (41, [60, 65, 69, 72]),  # F
]
NBARS = 31

# ---- pad ----
pad = np.zeros(N)
for b in range(NBARS):
    sec = section(b)
    if sec == "outro" and b > 28: continue
    bass, notes = CHORDS[b % 4] if sec != "outro" else CHORDS[0]
    length = BAR + 0.6 if sec != "outro" else 6.0
    n = int(length * SR); t = np.arange(n) / SR
    sig = np.zeros(n)
    for m in notes:
        for det in (-0.08, 0.0, 0.07):
            sig += saw(hz(m) * 2 ** (det / 12), t + rng.random(), harm=10, bright=0.6)
    e = env_adsr(n, 0.25, 1.0, 0.8, 0.5, length - 0.6)
    g = {"intro": 0.10 * min(1, (b + 1) / 3), "groove": 0.09, "full": 0.08, "break": 0.11, "outro": 0.11}[sec]
    add(pad, b * BAR, sig * e, g)

# ---- kick times / sidechain ----
kicks = []
for b in range(NBARS):
    sec = section(b)
    if sec in ("groove", "full"):
        kicks += [b * BAR + i * BEAT for i in range(4)]
kicks.append(56.0)
side = np.ones(N)
tt = np.arange(int(0.35 * SR)) / SR
duck = 1 - 0.65 * np.exp(-tt / 0.09)
for k in kicks:
    i = int(k * SR); seg = side[i:i + len(duck)]
    side[i:i + len(seg)] = np.minimum(seg, duck[:len(seg)])

# ---- bass (eighths, octave bounce) ----
bassbuf = np.zeros(N)
for b in range(NBARS):
    sec = section(b)
    if sec in ("intro",): continue
    root = CHORDS[b % 4][0] if sec != "outro" else 36
    if sec == "outro":
        if b == 28:
            n = int(4.0 * SR); t = np.arange(n) / SR
            add(bassbuf, 56.0, saw(hz(36), t, harm=8) * env_adsr(n, 0.005, 1.5, 0.3, 1.0, 3.0), 0.5)
        continue
    if sec == "break":
        n = int(BAR * SR); t = np.arange(n) / SR
        add(bassbuf, b * BAR, saw(hz(root), t, harm=6) * env_adsr(n, 0.05, 0.8, 0.6, 0.2, BAR - 0.2), 0.35)
        continue
    for i in range(8):
        m = root + (12 if i % 2 else 0)
        n = int(0.24 * SR); t = np.arange(n) / SR
        add(bassbuf, b * BAR + i * 0.25, saw(hz(m), t, harm=14) * env_adsr(n, 0.003, 0.12, 0.5, 0.03, 0.2), 0.42)

# ---- arp (16ths) ----
arpL = np.zeros(N); arpR = np.zeros(N)
PAT = [0, 2, 1, 3, 2, 0, 3, 1]
for b in range(NBARS):
    sec = section(b)
    notes = CHORDS[b % 4][1] if sec != "outro" else CHORDS[0][1]
    steps = 16 if sec != "outro" else (8 if b == 28 else 0)
    for i in range(steps):
        tstart = b * BAR + i * BAR / steps
        m = notes[PAT[i % 8]] + 12
        n = int(0.32 * SR); t = np.arange(n) / SR
        bright = {"intro": 0.15 + 0.3 * (b * BAR + i * 0.125) / 6, "groove": 0.5, "full": 0.7, "break": 0.4, "outro": 0.5}[sec]
        tone = np.zeros(n)
        f = hz(m)
        for k in range(1, int(min(16, SR / 2.2 / f)) + 1, 2):  # square-ish
            tone += np.sin(2 * np.pi * k * f * t) / k * np.exp(-(k - 1) / (8 * bright))
        tone *= np.exp(-t / 0.09)
        acc = 1.0 if i % 4 == 0 else 0.75
        g = 0.13 * acc * (min(1, 0.4 + (b * BAR + i * 0.125) / 6 * 0.6) if sec == "intro" else 1)
        pan = 0.25 if i % 2 else -0.25
        add(arpL, tstart, tone, g * np.sqrt(0.5 * (1 - pan)))
        add(arpR, tstart, tone, g * np.sqrt(0.5 * (1 + pan)))
# ping-pong dotted-eighth delay
d = int(0.375 * SR)
for _ in range(3):
    eL = np.zeros(N); eR = np.zeros(N)
    eL[d:] = arpR[:-d] * 0.35; eR[d:] = arpL[:-d] * 0.35
    arpL_d, arpR_d = eL, eR
arpL2 = arpL.copy(); arpR2 = arpR.copy()
src_l, src_r = arpL, arpR
for i in range(1, 4):
    sh = d * i
    g = 0.33 ** i
    if i % 2:
        arpL2[sh:] += src_r[:-sh] * g; arpR2[sh:] += src_l[:-sh] * g
    else:
        arpL2[sh:] += src_l[:-sh] * g; arpR2[sh:] += src_r[:-sh] * g

# ---- drums ----
def kick():
    n = int(0.45 * SR); t = np.arange(n) / SR
    f = 48 + 110 * np.exp(-t / 0.035)
    ph = 2 * np.pi * np.cumsum(f) / SR
    return np.sin(ph) * np.exp(-t / 0.16) + 0.3 * np.exp(-t / 0.004) * rng.standard_normal(n) * 0.2

def hat(open_=False):
    n = int((0.25 if open_ else 0.06) * SR); t = np.arange(n) / SR
    x = np.diff(rng.standard_normal(n + 1))
    x = np.diff(np.concatenate([[0], x]))
    return x * np.exp(-t / (0.08 if open_ else 0.018)) * 0.12

def clap():
    n = int(0.3 * SR); t = np.arange(n) / SR
    x = np.diff(rng.standard_normal(n + 1))
    e = np.exp(-t / 0.07)
    for o in (0.0, 0.011, 0.022):
        e += (t >= o) * np.exp(-np.maximum(t - o, 0) / 0.006) * 0.6
    return x * e * 0.15

def crash():
    n = int(2.2 * SR); t = np.arange(n) / SR
    x = np.diff(rng.standard_normal(n + 1))
    return x * np.exp(-t / 0.6) * 0.08

drums = np.zeros(N)
K = kick()
for k in kicks: add(drums, k, K, 0.9)
for b in range(NBARS):
    sec = section(b)
    for i in range(4):
        tb = b * BAR + i * BEAT
        if sec in ("groove", "full"):
            addst(tb + 0.25, hat(True), 0.7, 0.2)
        if sec == "full":
            if i in (1, 3): addst(tb, clap(), 1.0, 0.0)
            for s in (0.125, 0.375):
                addst(tb + s, hat(), 0.5, -0.3)
        if sec == "break" and b >= 19:
            addst(tb + 0.25, hat(), 0.5, 0.2)
for c in (6.0, 16.0, 42.0, 56.0):
    addst(c, crash(), 1.0, -0.2); addst(c, crash(), 1.0, 0.2)

# ---- risers into 6, 50, 64 (filtered noise swell) ----
def riser(length):
    n = int(length * SR); t = np.arange(n) / SR
    x = rng.standard_normal(n)
    y = np.zeros(n); acc = 0.0
    cut = 0.01 + 0.5 * (t / length) ** 2
    for i in range(n):
        acc += cut[i] * (x[i] - acc); y[i] = acc
    return y * (t / length) ** 2 * 0.35
for start, ln in ((4.0, 2.0), (38.0, 4.0), (52.0, 4.0)):
    r_ = riser(ln)
    addst(start, r_, 1.0, -0.3); addst(start, riser(ln), 1.0, 0.3)

# ---- simple reverb on pad + arp ----
irn = int(1.8 * SR); it = np.arange(irn) / SR
def rev(x, seed):
    ir = np.random.default_rng(seed).standard_normal(irn) * np.exp(-it / 0.5)
    ir[: int(0.02 * SR)] = 0
    m = 1 << int(np.ceil(np.log2(N + irn)))
    y = np.fft.irfft(np.fft.rfft(x, m) * np.fft.rfft(ir, m), m)[:N]
    return y / np.sqrt((ir ** 2).sum()) * 0.25

padd = pad * side
L += padd + rev(padd + arpL2, 1) + arpL2 * side ** 0.5 + bassbuf * side + drums
R += padd + rev(padd + arpR2, 2) + arpR2 * side ** 0.5 + bassbuf * side + drums

# master: gentle limiter + fade tail to silence at 70 s
mix = np.stack([L, R])
mix = np.tanh(mix * 1.2) / 1.2
t = np.arange(N) / SR
mix *= np.clip((62.0 - t) / 4.5, 0, 1) ** 1.5 * (t > 56) + (t <= 56)
mix *= np.clip(t / 0.05, 0, 1)
mix /= np.abs(mix).max() / 0.89
out = (mix.T * 32767).astype("<i2")
import wave
with wave.open(sys.argv[1], "wb") as w:
    w.setnchannels(2); w.setsampwidth(2); w.setframerate(SR); w.writeframes(out.tobytes())
print("ok")
