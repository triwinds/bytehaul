#!/usr/bin/env python3
"""Same-origin multi-IP experiment, using only loopback HTTP and DNS sockets."""
import argparse
import csv
import hashlib
import json
import os
import socket
import socketserver
import statistics
import struct
import subprocess
import threading
import time
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from compare_local import ROOT, MIB, CHUNK, fixture_bytes, expected_hash

IPS = ['127.0.0.2', '127.0.0.3', '127.0.0.4']
DNS_IP = '127.0.0.10'
DNS_PORT = 15353


class DNSHandler(socketserver.BaseRequestHandler):
    def handle(self):
        data, sock = self.request
        if len(data) < 17:
            return
        end = 12
        labels = []
        while end < len(data) and data[end]:
            size = data[end]
            labels.append(data[end + 1:end + 1 + size].decode('ascii'))
            end += size + 1
        end += 1
        if end + 4 > len(data):
            return
        qtype, qclass = struct.unpack('!HH', data[end:end + 4])
        name = '.'.join(labels)
        addresses = IPS if name == 'fixture.test' and qtype == 1 and qclass == 1 else []
        answers = b''.join(b'\xc0\x0c' + struct.pack('!HHIH', 1, 1, 60, 4)
                           + socket.inet_aton(ip) for ip in addresses)
        packet = data[:2] + struct.pack('!HHHHH', 0x8180, 1, len(addresses), 0, 0)
        sock.sendto(packet + data[12:end + 4] + answers, self.client_address)
        self.server.events.append({'name': name, 'type': qtype, 'answers': addresses})


class State:
    def __init__(self, scenario, size):
        self.scenario, self.size = scenario, size
        self.started = time.perf_counter()
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.events = []
        self.connections = 0

    def ms(self):
        return round((time.perf_counter() - self.started) * 1000, 3)


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def setup(self):
        super().setup()
        self.connection.settimeout(6)
        with self.server.state.lock:
            self.server.state.connections += 1
            self.connection_id = self.server.state.connections

    def log_message(self, *_):
        pass

    def handle(self):
        try:
            super().handle()
        except (ConnectionAbortedError, ConnectionResetError, BrokenPipeError):
            # Expected when the client cancels another keep-alive request
            # after detecting a changed object or exhausting its retry budget.
            self.close_connection = True

    def do_HEAD(self):
        self.respond(False)

    def do_GET(self):
        self.respond(True)

    def respond(self, body):
        state = self.server.state
        ip = self.connection.getsockname()[0]
        start, end = 0, state.size
        value = self.headers.get('Range')
        if value:
            first, last = value.removeprefix('bytes=').split('-')
            start, end = int(first), min(int(last) + 1 if last else state.size, state.size)
        behavior = 'normal'
        # Bootstrap metadata/first piece normally, then fault an IP persistently.
        if start > 0:
            if state.scenario == 'all-stall':
                behavior = 'stall'
            elif state.scenario == 'all-slow':
                behavior = 'slow'
            elif ip == IPS[0] and state.scenario in ('slow', 'stall', 'changed'):
                behavior = state.scenario
        event = dict(ip=ip, connection_id=self.connection_id, method=self.command,
                     start=start, end=end, behavior=behavior, started_ms=state.ms(),
                     bytes_sent=0, disconnected=False)
        with state.lock:
            state.events.append(event)
        try:
            self.send_response(206 if value else 200)
            self.send_header('Content-Length', str(end - start))
            self.send_header('Accept-Ranges', 'bytes')
            self.send_header('ETag', '"v2"' if behavior == 'changed' else '"v1"')
            if value:
                self.send_header('Content-Range', f'bytes {start}-{end - 1}/{state.size}')
            self.end_headers()
            if not body:
                return
            if behavior == 'stall' and state.stop.wait(5):
                return
            rate = 256 * 1024 if behavior == 'slow' else 16 * MIB
            at = start
            while at < end and not state.stop.is_set():
                count = min(CHUNK, end - at)
                data = fixture_bytes(at, count)
                if behavior == 'changed':
                    data = bytes(byte ^ 255 for byte in data)
                self.wfile.write(data)
                self.wfile.flush()
                event['bytes_sent'] += count
                at += count
                state.stop.wait(count / rate)
        except (OSError, ConnectionError):
            event['disconnected'] = True
            self.close_connection = True
        finally:
            event['ended_ms'] = state.ms()


class HTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def server_bind(self):
        # HTTPServer normally reverse-resolves the loopback aliases, adding
        # unrelated system-DNS delays before the experiment starts.
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address


def run(args, root, scenario, tool, repeat):
    folder = root / f'{scenario}-r{repeat}-{tool}'
    folder.mkdir()
    state = State(scenario, args.size_mib * MIB)
    dns = socketserver.UDPServer((DNS_IP, args.dns_port), DNSHandler)
    dns.events = []
    servers, threads = [], []
    reserved = None
    try:
        # Pick an ephemeral port on a healthy IP, then bind the other candidates.
        first = HTTPServer((IPS[1], 0), Handler)
        servers.append(first)
        port = first.server_port
        for ip in (IPS[0], IPS[2]):
            if scenario == 'refused' and ip == IPS[0]:
                reserved = socket.socket()
                reserved.bind((ip, port))  # bound but not listening
            else:
                servers.append(HTTPServer((ip, port), Handler))
        for server in servers:
            server.state = state
        for server in [dns, *servers]:
            thread = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': .05}, daemon=True)
            thread.start()
            threads.append(thread)
        output = folder / 'fixture.bin'
        url = f'http://fixture.test:{port}/fixture.bin'
        if tool == 'bytehaul':
            cmd = [str(Path(args.bytehaul).resolve()), url, str(output), '8', '--multi-ip',
                   '--dns-server', f'{DNS_IP}:{args.dns_port}', '--dynamic-min-split-size', str(MIB),
                   '--dynamic-max-request-size', str(4 * MIB), '--connect-timeout-secs', '3',
                   '--read-timeout-secs', '2', '--headers-timeout-secs', '3', '--log-level', 'debug']
        else:
            cmd = [str(Path(args.aria2).resolve()), '--no-conf=true', '--no-netrc=true',
                   '--async-dns=true', f'--async-dns-server={DNS_IP}', '--disable-ipv6=true',
                   '--split=8', '--max-connection-per-server=8', '--min-split-size=1M',
                   '--piece-length=1M', '--file-allocation=none', '--connect-timeout=3',
                   '--timeout=2', '--max-tries=3', '--retry-wait=1', '--summary-interval=0',
                   '--console-log-level=debug', f'--dir={folder}', '--out=fixture.bin', url]
        (folder / 'command.json').write_text(json.dumps(cmd, indent=2), encoding='utf-8')
        env = {key: value for key, value in os.environ.items()
               if key.lower() not in ('http_proxy', 'https_proxy', 'all_proxy', 'no_proxy')}
        started = time.perf_counter()
        try:
            with (folder / 'stdout.log').open('wb') as out, (folder / 'stderr.log').open('wb') as err:
                status = subprocess.run(cmd, stdout=out, stderr=err, env=env, timeout=35).returncode
        except subprocess.TimeoutExpired:
            status = 'timeout'
        seconds = round(time.perf_counter() - started, 3)
    finally:
        state.stop.set()
        for server in [dns, *servers]:
            server.shutdown() if threads else None
            server.server_close()
        for thread in threads:
            thread.join(timeout=2)
        if reserved:
            reserved.close()
    (folder / 'server.jsonl').write_text(''.join(json.dumps(e) + '\n' for e in state.events), encoding='utf-8')
    (folder / 'dns.json').write_text(json.dumps(dns.events, indent=2), encoding='utf-8')
    with (folder / 'split_ips.csv').open('w', newline='', encoding='utf-8') as stream:
        writer = csv.DictWriter(stream, fieldnames=['ip', 'connection_id', 'method', 'start', 'end',
                                                  'behavior', 'started_ms', 'bytes_sent', 'disconnected', 'ended_ms'])
        writer.writeheader()
        writer.writerows(state.events)
    verified = False
    if status == 0 and output.exists():
        with output.open('rb') as stream:
            verified = hashlib.file_digest(stream, 'sha256').hexdigest() == expected_hash(state.size)
    counts = Counter(e['ip'] for e in state.events if e['method'] == 'GET')
    connections = {e['connection_id'] for e in state.events}
    return dict(scenario=scenario, repeat=repeat, tool=tool, status=status, seconds=seconds,
                verified=verified, requests=sum(counts.values()), connections=len(connections),
                reused_requests=len(state.events) - len(connections), ips=json.dumps(dict(counts)),
                fault_requests=sum(e['behavior'] != 'normal' for e in state.events))


def summarize(root):
    root = Path(root)
    with (root / 'results.csv').open(newline='', encoding='utf-8') as stream:
        rows = list(csv.DictReader(stream))
    lines = ['# Local same-origin multi-IP results', '',
             'See metadata.json for binaries, parameters and fault semantics. All IPs serve the same domain and port.', '',
             '| Scenario | Tool | SHA-256 verified | Median seconds | Requests median | Reused requests median |',
             '|---|---|---:|---:|---:|---:|']
    for scenario, tool in dict.fromkeys((r['scenario'], r['tool']) for r in rows):
        selected = [r for r in rows if r['scenario'] == scenario and r['tool'] == tool]
        median = lambda key: statistics.median(float(r[key]) for r in selected)
        lines.append(f"| {scenario} | {tool} | {sum(r['verified'] == 'True' for r in selected)}/{len(selected)} | "
                     f"{median('seconds'):.3f} | {median('requests'):g} | {median('reused_requests'):g} |")
    lines += ['', 'all-stall should terminate with an error; changed should reject inconsistent validators, not produce a successful mixed file.',
              'Per-run split_ips.csv and server.jsonl record actual destination IP, connection ID, byte range and timestamps.',
              'Reuse is measured by subsequent HTTP requests on the same accepted TCP connection.', '']
    (root / 'report.md').write_text('\n'.join(lines), encoding='utf-8')
    combined = []
    for row in rows:
        folder = root / f"{row['scenario']}-r{row['repeat']}-{row['tool']}"
        with (folder / 'split_ips.csv').open(newline='', encoding='utf-8') as stream:
            events = list(csv.DictReader(stream))
        origin = min((float(e['started_ms']) for e in events), default=0)
        for event in events:
            combined.append(dict(scenario=row['scenario'], repeat=row['repeat'], tool=row['tool'],
                                 relative_start_ms=round(float(event['started_ms']) - origin, 3), **event))
    if combined:
        with (root / 'split_ips.csv').open('w', newline='', encoding='utf-8') as stream:
            writer = csv.DictWriter(stream, fieldnames=list(combined[0]))
            writer.writeheader()
            writer.writerows(combined)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--aria2', required=True)
    parser.add_argument('--bytehaul', default=ROOT / 'target/release/examples/public_compare.exe')
    parser.add_argument('--rounds', type=int, default=2)
    parser.add_argument('--tools', nargs='+', choices=['bytehaul', 'aria2'], default=['bytehaul'])
    parser.add_argument('--dns-port', type=int, default=DNS_PORT)
    parser.add_argument('--size-mib', type=int, default=64)
    parser.add_argument('--scenarios', nargs='+', default=['normal', 'refused', 'slow', 'stall', 'all-stall', 'changed'],
                        choices=['normal', 'refused', 'slow', 'stall', 'all-stall', 'all-slow', 'changed'])
    args = parser.parse_args()
    if 'aria2' in args.tools and args.dns_port != 53:
        parser.error('This aria2 build requires --dns-port 53; it ignores DNS port suffixes.')
    root = ROOT / 'target/multi-ip-compare' / time.strftime('%Y%m%d-%H%M%S')
    root.mkdir(parents=True)
    (root / 'metadata.json').write_text(json.dumps(dict(args=vars(args), ips=IPS, dns=f'{DNS_IP}:{args.dns_port}',
        semantics='Fault IP .2 persistently after bootstrap; all-stall/all-slow fault all IPs after bootstrap.',
        binaries={str(path): hashlib.sha256(Path(path).read_bytes()).hexdigest()
                  for path in (args.bytehaul, args.aria2)}), indent=2, default=str), encoding='utf-8')
    print(root, flush=True)
    rows = []
    for scenario in args.scenarios:
        for repeat in range(1, args.rounds + 1):
            for tool in (args.tools if repeat % 2 else list(reversed(args.tools))):
                row = run(args, root, scenario, tool, repeat)
                rows.append(row)
                print(json.dumps(row), flush=True)
                with (root / 'results.csv').open('w', newline='', encoding='utf-8') as stream:
                    writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
                    writer.writeheader()
                    writer.writerows(rows)
    summarize(root)


if __name__ == '__main__':
    main()
