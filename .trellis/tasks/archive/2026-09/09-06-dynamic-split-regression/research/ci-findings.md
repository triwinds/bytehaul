# Verified failure and prevention

Run https://github.com/triwinds/bytehaul/actions/runs/34040546169 at7ac3b2e has4 successful jobs (Linux coverage, Python, UbuntuRust, macOSRust). Windows job101506376704 fails only Run unit tests:364 passed/1failed. Browser authenticated log shows test_run_multi_worker_dynamic_split_reduces_tail_latency, multi.rs:1920, unsplit466.0892ms and split320.0947ms. Exact threshold split+150ms<unsplit fails by4.0055ms. Both output-content assertions passed first.

The helper injects450ms delay for ranges>256bytes,120ms otherwise. Actual wall time includes scheduler, network and filesystem overhead; logs establish a brittle fixed improvement threshold, not the specific source of the extra overhead. gitblame traces this test to676fa36f (May24), rather than the most recent coverage tests.

Replace timing comparison with gated concurrent request arrival, exact disjoint ranges, patterned output and Completed progress. Preserve an unsplit control case. A bounded timeout detects deadlocks; it is not a speedup target. Testing-and-quality.md captures this contract.
