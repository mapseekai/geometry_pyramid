import inspect
from pathlib import Path
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


class SendTests(unittest.TestCase):
    class _ResettingWriter:
        def write(self, _body):
            raise ConnectionResetError("client closed")

    class _HandlerStub:
        def __init__(self):
            self.status = None
            self.headers = []
            self.wfile = SendTests._ResettingWriter()
            self.close_connection = False

        def send_response(self, code):
            self.status = code

        def send_header(self, key, value):
            self.headers.append((key, value))

        def end_headers(self):
            pass

    def test_send_ignores_client_disconnect_while_writing_body(self):
        handler = self._HandlerStub()

        server.Handler._send(handler, 200, b"tile", "application/vnd.mapbox-vector-tile")

        self.assertEqual(handler.status, 200)
        self.assertTrue(handler.close_connection)


class FetchTileTests(unittest.TestCase):
    def tearDown(self):
        server._CONN = None

    def test_requires_initialized_connection(self):
        server._CONN = None

        with self.assertRaisesRegex(RuntimeError, "database connection is not initialized"):
            server._fetch_tile("demo", 0, 0, 0)


class HandlerSignatureTests(unittest.TestCase):
    def test_log_message_matches_base_signature(self):
        base_parameters = tuple(
            inspect.signature(server.BaseHTTPRequestHandler.log_message).parameters
        )
        handler_parameters = tuple(inspect.signature(server.Handler.log_message).parameters)

        self.assertEqual(handler_parameters, base_parameters)


class StaticDemoTests(unittest.TestCase):
    def test_online_basemap_is_opt_in_and_toggleable(self):
        html = Path(__file__).with_name("index.html").read_text()

        self.assertIn('id="basemap-toggle"', html)
        self.assertNotRegex(html, r'id="basemap-toggle"[^>]*checked')
        self.assertIn("https://tile.openstreetmap.org/{z}/{x}/{y}.png", html)
        self.assertIn("function setBasemapVisible", html)
        self.assertIn("map.addSource(BASEMAP_SOURCE_ID", html)
        self.assertIn("map.removeSource(BASEMAP_SOURCE_ID", html)


if __name__ == "__main__":
    unittest.main()
