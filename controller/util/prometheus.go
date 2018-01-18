package util

import (
	grpc_prometheus "github.com/grpc-ecosystem/go-grpc-prometheus"
	"github.com/prometheus/client_golang/prometheus"
	"google.golang.org/grpc"
)

// returns a grpc server pre-configured with prometheus interceptors
func NewGrpcServer() *grpc.Server {
	server := grpc.NewServer(
		grpc.UnaryInterceptor(grpc_prometheus.UnaryServerInterceptor),
		grpc.StreamInterceptor(grpc_prometheus.StreamServerInterceptor),
	)

	grpc_prometheus.EnableHandlingTimeHistogram()
	grpc_prometheus.Register(server)
	return server
}

// define latency buckets to record (seconds)
var RequestDurationBucketsSeconds = append(append(append(append(
	prometheus.LinearBuckets(0.01, 0.01, 5),
	prometheus.LinearBuckets(0.1, 0.1, 5)...),
	prometheus.LinearBuckets(1, 1, 5)...),
	prometheus.LinearBuckets(10, 10, 5)...),
)

// define response size buckets (bytes)
var ResponseSizeBuckets = append(append(append(append(
	prometheus.LinearBuckets(100, 100, 5),
	prometheus.LinearBuckets(1000, 1000, 5)...),
	prometheus.LinearBuckets(10000, 10000, 5)...),
	prometheus.LinearBuckets(1000000, 1000000, 5)...),
)
