// Conformance client: the real AWS Go SDK (aws-sdk-go-v2) against a
// WireMirage S3 mock with a reusable latency/throttle-injection handler.
//
// Proves, from a real client's perspective:
//   [1] a non-matching key is served fast (passthrough)
//   [2] a "slow" key has injected latency (observably slower)
//   [3] a "throttled" key (mock returns 503 SlowDown for the first 2 attempts)
//       still SUCCEEDS — because the SDK auto-retries/backs off and recovers
//   [4] the same throttled shape, with retries disabled, SURFACES the error —
//       confirming it's the SDK's retry that recovers in [3], i.e. the mock is
//       genuinely emulating a partial, recoverable slowdown
package main

import (
	"context"
	"fmt"
	"io"
	"os"
	"strings"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

func base() string {
	if v := os.Getenv("WM_BASE"); v != "" {
		return v
	}
	return "http://localhost:8080"
}

func newClient(maxAttempts int) *s3.Client {
	cfg, err := config.LoadDefaultConfig(context.TODO(),
		config.WithRegion("us-east-1"),
		config.WithCredentialsProvider(
			credentials.NewStaticCredentialsProvider("test", "test", "")),
	)
	if err != nil {
		fatal("load config: %v", err)
	}
	return s3.NewFromConfig(cfg, func(o *s3.Options) {
		o.BaseEndpoint = aws.String(base())
		o.UsePathStyle = true // path-style: GET /{bucket}/{key}
		if maxAttempts > 0 {
			o.RetryMaxAttempts = maxAttempts
		}
	})
}

func get(c *s3.Client, bucket, key string) (string, time.Duration, error) {
	t0 := time.Now()
	out, err := c.GetObject(context.TODO(), &s3.GetObjectInput{
		Bucket: aws.String(bucket), Key: aws.String(key),
	})
	d := time.Since(t0)
	if err != nil {
		return "", d, err
	}
	defer out.Body.Close()
	b, _ := io.ReadAll(out.Body)
	return string(b), d, nil
}

func main() {
	def := newClient(0)         // default retryer (recovers from throttling)
	noRetry := newClient(1)     // retries disabled (control)

	// [1] fast passthrough
	body, fastDur, err := get(def, "fastbucket", "obj.txt")
	if err != nil {
		fatal("[1] fast key errored: %v", err)
	}
	if !strings.Contains(body, "wiremirage-mock-object") {
		fatal("[1] unexpected body: %q", body)
	}
	fmt.Printf("[1] fast key: ok in %v\n", fastDur.Round(time.Millisecond))

	// [2] injected latency
	_, slowDur, err := get(def, "slowbucket", "obj.txt")
	if err != nil {
		fatal("[2] slow key errored: %v", err)
	}
	fmt.Printf("[2] slow key: ok in %v (fast was %v)\n",
		slowDur.Round(time.Millisecond), fastDur.Round(time.Millisecond))
	if slowDur < 700*time.Millisecond {
		fatal("[2] expected injected latency (~800ms), got %v", slowDur)
	}

	// [3] throttled but recovers via SDK retry/backoff
	body, thrDur, err := get(def, "throttlebucket", "a.txt")
	if err != nil {
		fatal("[3] throttled key did NOT recover (SDK should retry past SlowDown): %v", err)
	}
	if !strings.Contains(body, "wiremirage-mock-object") {
		fatal("[3] unexpected body after recovery: %q", body)
	}
	fmt.Printf("[3] throttled key: recovered via retry, ok in %v\n",
		thrDur.Round(time.Millisecond))

	// [4] control: retries disabled -> the throttling error surfaces
	_, _, err = get(noRetry, "throttlebucket", "b.txt")
	if err == nil {
		fatal("[4] expected SlowDown to surface with retries disabled, but it succeeded")
	}
	if !strings.Contains(err.Error(), "SlowDown") && !strings.Contains(err.Error(), "503") {
		fatal("[4] error did not look like a throttle: %v", err)
	}
	fmt.Printf("[4] no-retry control: error surfaced as expected (%s)\n",
		firstLine(err.Error()))

	fmt.Println("\nALL CONFORMANCE CHECKS PASSED")
}

func firstLine(s string) string {
	if i := strings.IndexByte(s, '\n'); i >= 0 {
		return s[:i]
	}
	if len(s) > 120 {
		return s[:120]
	}
	return s
}

func fatal(format string, a ...any) {
	fmt.Fprintf(os.Stderr, "FAIL: "+format+"\n", a...)
	os.Exit(1)
}
