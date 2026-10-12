"""Read-only evidence plumbing for the independent N1 canary oracle."""
import json

VERSION = "0.1.0"


class Unknown(ValueError):
    """Evidence cannot establish the requested fact."""


def loads(raw):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise Unknown("duplicate_json_key")
            result[key] = value
        return result

    def constant(_):
        raise Unknown("nonfinite_json")

    return json.loads(raw, object_pairs_hook=pairs, parse_constant=constant)
