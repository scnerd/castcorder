# /// script
# dependencies = ["jiwer", "whisper-normalizer"]
# ///
# Score RESULT lines (results.jsonl) against data/clip.txt and print a markdown table.
# Usage: uv run score.py results.jsonl data/clip.txt
import json, sys
import jiwer
from whisper_normalizer.english import EnglishTextNormalizer

norm = EnglishTextNormalizer()
ref = norm(open(sys.argv[2], encoding="ascii").read())


def wer(h):
    return 100 * jiwer.wer(ref, norm(h) or "x")


print("| run | load s | RTF full | WER full | 5 s chunk med/max s | WER 5 s | 10 s chunk med/max s | WER 10 s | peak RAM MB | peak VRAM MB |")
print("|---|---|---|---|---|---|---|---|---|---|")
for line in open(sys.argv[1], encoding="ascii"):
    r = json.loads(line)
    print(
        f"| {r['label']} | {r['load_s']:.2f} | {r['rtf']:.3f} | {wer(r['full_text']):.1f} | "
        f"{r['c5_med_s']:.2f}/{r['c5_max_s']:.2f} | {wer(r['c5_text']):.1f} | "
        f"{r['c10_med_s']:.2f}/{r['c10_max_s']:.2f} | {wer(r['c10_text']):.1f} | "
        f"{r['peak_rss_mb']:.0f} | {r.get('vram_mb', '-')} |"
    )
