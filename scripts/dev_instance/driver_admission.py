"""Bind an instance Driver to declared authors before importing relay assertions."""
from dataclasses import dataclass
import time
import uuid

from . import identity
from .files import InstanceError, instance_root
from .manifest import read_json, validate
from .profile import snowflake

# Existing relay author defaults are release identities, never instance fallbacks.
OPERATING_BOT_IDS = frozenset({'1479017284805722200', '1474932782395293736', '1479425196824989758',
                               '1481522187197218816'})


class IdentityRefused(InstanceError):
    pass


@dataclass(frozen=True)
class Admission:
    manifest: dict
    run: dict
    observed: list


def admit(base, instance_id, deadline, request=None):
    request = request or identity.request_json
    try:
        root = instance_root(base, instance_id)
        owner = read_json(root / 'runtime/resources-owner.json')
        sockets = {path: tuple(value) for path, value in owner['sockets'].items()}
        manifest = validate(base, instance_id, {}, check_ports=False, live_sockets=sockets)
        bots = manifest['profile']['bots']
        authors = [snowflake(bots[role]) for role in ('claude', 'codex', 'driver')]
        if len(set(authors)) != 3 or OPERATING_BOT_IDS.intersection(authors):
            raise IdentityRefused('instance author IDs are invalid')
        run = read_json(root / 'runtime/instance-run.json')
        if (run['instance_id'] != instance_id or run['head'] != manifest['head']
                or run['port'] != manifest['api']['port'] or run['live_ready'] is not True
                or run['verified'] is not True or type(run['pid']) is not int or run['pid'] <= 1
                or str(uuid.UUID(run['nonce'])) != run['nonce']):
            raise IdentityRefused('instance runtime receipt mismatch')
        expected = {item['name']: snowflake(item['application_id']) for item in run['identities']}
        frame = {'kind':'admission','schema':1,'identities':expected}
        identity.parse_admission(frame)
        if (OPERATING_BOT_IDS.intersection(expected.values())
                or expected['notify_bot_token'] != bots['driver']):
            raise IdentityRefused('instance Driver role mismatch')
        observed = identity.probe(root, frame, deadline,
            request=lambda token,end: request('https://discord.com/api/v10/users/@me',end,token).get('id'),
            declared_roles=bots)
        if observed != run['identities']:
            raise IdentityRefused('credential identity receipt mismatch')
        current = request(manifest['api']['self_url']+'/api/health', deadline)['test_instance']
        if (current.get('instance_id') != instance_id or current.get('nonce') != run['nonce']
                or type(current.get('pid')) is not int or current['pid'] != run['pid']
                or time.monotonic() >= deadline):
            raise IdentityRefused('current instance health mismatch')
        return Admission(manifest, run, observed)
    except (InstanceError, OSError, ValueError, TypeError, KeyError, AttributeError, StopIteration):
        raise IdentityRefused('instance identity admission refused') from None
