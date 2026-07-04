import sys
from pathlib import Path

# Make the src-layout package and the fixtures helper importable without install.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
