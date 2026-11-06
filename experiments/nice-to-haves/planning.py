"""Synthetic planning exercises, never estimates inferred from a running host."""

import argparse
import json
import math
import random
import sys
from pathlib import Path


def integer(value, low=0, high=10**12):
    if type(value) is not int or not low <= value <= high:
        raise ValueError(f'integer in [{low}, {high}] required')
    return value


def number(value):
    if type(value) not in (int, float) or not math.isfinite(value) or not 0 <= value <= 10**12:
        raise ValueError('finite nonnegative number <= 1e12 required')
    return value


def calculate(data):
    if data['basis'] != 'synthetic':
        raise ValueError('these exercises require basis=synthetic')
    kind = data['kind']
    if kind == 'amplification':
        block = integer(data['block_bytes'], 1)
        metadata = integer(data['metadata_bytes_per_file'])
        sizes = data['file_bytes']
        if not isinstance(sizes, list) or len(sizes) > 10000:
            raise ValueError('at most 10000 file lengths required')
        logical = sum(integer(n) for n in sizes)
        allocated = sum(((n + block - 1) // block) * block for n in sizes)
        total = allocated + len(sizes) * metadata
        result = dict(logical_bytes=logical, data_blocks_bytes=allocated,
                      metadata_bytes=len(sizes) * metadata, total_bytes=total,
                      amplification=None if not logical else total / logical)
    elif kind == 'cardinality':
        counts = data['label_value_counts']
        if not isinstance(counts, list) or len(counts) > 32:
            raise ValueError('at most 32 label cardinalities required')
        series = integer(data['metric_families'], 1) * math.prod(integer(n) for n in counts)
        series *= integer(data['series_per_labelset'], 1)
        result = dict(series_upper_bound=series,
                      over_budget=series > integer(data['series_budget']),
                      exceeds_signed_64bit=series > 2**63 - 1)
    elif kind == 'cost':
        hours = number(data['hours'])
        nodes = integer(data['nodes'])
        energy = nodes * number(data['watts_per_node']) * hours / 1000
        result = dict(kwh=energy, electricity_cost=energy * number(data['cost_per_kwh']),
                      cloud_cost=nodes * hours * number(data['cloud_cost_per_node_hour']))
    elif kind == 'queue':
        jobs = data['jobs']
        if not isinstance(jobs, list) or len(jobs) > 10000:
            raise ValueError('at most 10000 jobs required')
        end = previous = 0
        waits, finishes = [], []
        for job in jobs:
            arrival, service = integer(job['arrival_ms']), integer(job['service_ms'])
            if arrival < previous:
                raise ValueError('jobs must be ordered by arrival')
            start = max(arrival, end)
            waits.append(start - arrival)
            end = start + service
            finishes.append(end)
            previous = arrival
        result = dict(wait_ms=waits, finish_ms=finishes,
                      mean_wait_ms=sum(waits) / len(waits) if waits else None)
    elif kind == 'order':
        pairs = integer(data['pairs'], 2, 10000)
        if pairs % 2:
            raise ValueError('even pair count required for exact AB/BA balance')
        rng = random.Random(integer(data['seed']))
        order = ['AB'] * (pairs // 2) + ['BA'] * (pairs // 2)
        rng.shuffle(order)
        result = dict(pairs=order, seed=data['seed'])
    elif kind == 'popularity':
        objects = integer(data['objects'], 1, 10000)
        requests = integer(data['requests'], 0, 10000)
        rng = random.Random(integer(data['seed']))
        distribution = data['distribution']
        if distribution == 'uniform':
            weights = [1] * objects
        elif distribution == 'zipf':
            exponent = number(data['exponent'])
            if exponent > 10:
                raise ValueError('Zipf exponent must be <= 10')
            weights = [1 / (rank + 1)**exponent for rank in range(objects)]
        elif distribution == 'hotset':
            hot = integer(data['hot_objects'], 1, objects - 1)
            share = number(data['hot_share'])
            if share > 1:
                raise ValueError('hot_share must be in [0, 1]')
            weights = [share / hot] * hot + [(1 - share) / (objects - hot)] * (objects - hot)
        else:
            raise ValueError('unknown distribution')
        sequence = rng.choices(range(objects), weights=weights, k=requests)
        counts = [0] * objects
        for object_id in sequence:
            counts[object_id] += 1
        result = dict(sequence=sequence, counts=counts, seed=data['seed'])
    else:
        raise ValueError('unknown exercise kind')
    return dict(basis='synthetic', kind=kind, result=result)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('input', type=Path)
    args = parser.parse_args()
    try:
        print(json.dumps(calculate(json.loads(args.input.read_text())), indent=2,
                         sort_keys=True, allow_nan=False))
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(f'invalid exercise: {error}', file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    sys.exit(main())
