import unittest

from migrate_visibility import convert


class MigrationTests(unittest.TestCase):
    def test_preserves_explicit_hidden_players(self):
        result = convert(
            b'{"hidden-players":["00000000-0000-0000-0000-000000000001"],"other":{}}'
        )
        self.assertEqual(result, {
            "version": 1,
            "hiddenPlayers": ["00000000-0000-0000-0000-000000000001"],
            "vanishSource": "pumpkin-observer-hides",
        })
        self.assertEqual(convert(b'{"hidden-players":[]}')["hiddenPlayers"], [])

    def test_missing_or_ambiguous_privacy_does_not_become_public(self):
        for source in [
            b'{}',
            b'{"hidden-players":null}',
            b'{"hidden-players":[false]}',
            b'{"hidden-players":["00000000-0000-0000-0000-00000000000A"]}',
            b'{"hidden-players":[],"hidden-players":[]}',
            b'{"hidden-players":[],"other":NaN}',
            b'{"hidden-players":["00000000-0000-0000-0000-000000000001",'
            b'"00000000-0000-0000-0000-000000000001"]}',
        ]:
            with self.subTest(source=source), self.assertRaises(ValueError):
                convert(source)


if __name__ == "__main__":
    unittest.main()
