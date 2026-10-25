"""Offline tests of eval.py's helpers (no host, no GPU): python3 -m unittest discover eval."""
import struct
import unittest
import zlib

import eval as ev


class Helpers(unittest.TestCase):
    def test_png_is_valid(self):
        data = ev.png(3, 2, lambda x, y: (255, 0, 0))
        self.assertEqual(data[:8], b"\x89PNG\r\n\x1a\n")
        width, height = struct.unpack(">II", data[16:24])
        self.assertEqual((width, height), (3, 2))
        idat = data.index(b"IDAT")
        size = struct.unpack(">I", data[idat - 4:idat])[0]
        rows = zlib.decompress(data[idat + 4:idat + 4 + size])
        self.assertEqual(rows, (b"\0" + b"\xff\0\0" * 3) * 2)

    def test_rust_durations(self):
        self.assertAlmostEqual(ev.seconds("45.20ms"), 0.0452)
        self.assertAlmostEqual(ev.seconds("1.20s"), 1.2)
        self.assertAlmostEqual(ev.seconds("950.00µs"), 0.00095)
        self.assertNotEqual(ev.seconds("soon"), ev.seconds("soon"), "unparsable is NaN")

    def test_engine_stats_line(self):
        line = "12 prompt tokens in 45.20ms (265.5 tok/s); 30 tokens in 280.00ms (107.1 tok/s), by tachyon serve"
        m = ev.STATS.search(line)
        self.assertEqual((m[1], m[3], m[4], m[6]), ("12", "265.5", "30", "107.1"))

    def test_cosine(self):
        self.assertAlmostEqual(ev.cosine([1, 0], [1, 0]), 1.0)
        self.assertAlmostEqual(ev.cosine([1, 0], [0, 1]), 0.0)

    def test_cases_are_consistent(self):
        intents = set(ev.INTENT["criteria"])
        self.assertTrue(all(intent in intents for intent, _, _ in ev.MESSAGES))
        self.assertTrue(all(0 <= want < len(ev.PRODUCTS) for _, want in ev.QUERIES))


if __name__ == "__main__":
    unittest.main()
