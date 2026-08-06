class SwapDepositBroadcastStatus {
  const SwapDepositBroadcastStatus._();

  static const broadcasted = 'broadcasted';
  static const pendingBroadcast = 'pending_broadcast';
  static const partialBroadcast = 'partial_broadcast';
  static const broadcastUnknown = 'broadcast_unknown';
  static const broadcastedStorageFailed = 'broadcasted_storage_failed';
}

class SwapDepositBroadcastResult {
  const SwapDepositBroadcastResult({
    required this.txHash,
    required this.status,
    this.message,
  });

  final String txHash;
  final String status;
  final String? message;

  bool get isCertain => status == SwapDepositBroadcastStatus.broadcasted;
}
