import unittest
import server


class ParseTilePathTests(unittest.TestCase):
    def test_valid_path(self):
        self.assertEqual(
            server.parse_tile_path("/tiles/demo/3/4/5.pbf"),
            ("demo", 3, 4, 5),
        )

    def test_name_with_underscore_and_digits(self):
        self.assertEqual(
            server.parse_tile_path("/tiles/_gp_1/0/0/0.pbf"),
            ("_gp_1", 0, 0, 0),
        )

    def test_rejects_illegal_name(self):
        self.assertIsNone(server.parse_tile_path("/tiles/foo;DROP/0/0/0.pbf"))

    def test_rejects_non_integer(self):
        self.assertIsNone(server.parse_tile_path("/tiles/demo/x/0/0.pbf"))

    def test_rejects_wrong_extension(self):
        self.assertIsNone(server.parse_tile_path("/tiles/demo/0/0/0.png"))

    def test_rejects_root(self):
        self.assertIsNone(server.parse_tile_path("/"))

    def test_rejects_missing_coords(self):
        self.assertIsNone(server.parse_tile_path("/tiles/demo/0/0.pbf"))


if __name__ == "__main__":
    unittest.main()
