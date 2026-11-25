# /// script
# dependencies = ["pyarrow", "soundfile", "numpy"]
# ///
import io, sys
import numpy as np, pyarrow.parquet as pq, soundfile as sf

src, out_wav, out_txt = sys.argv[1], sys.argv[2], sys.argv[3]
t = pq.read_table(src).to_pylist()
pieces, texts, total = [], [], 0.0
gap = np.zeros(int(16000 * 0.4), dtype=np.float32)
for row in t:
    a, sr = sf.read(io.BytesIO(row["audio"]["bytes"]), dtype="float32")
    assert sr == 16000
    pieces += [a, gap]
    texts.append(row["text"])
    total += len(a) / 16000 + 0.4
    if total >= 120:
        break
sf.write(out_wav, np.concatenate(pieces), 16000, subtype="PCM_16")
open(out_txt, "w", encoding="ascii").write(" ".join(texts) + "\n")
print("utterances", len(texts), "seconds", round(total, 1))
