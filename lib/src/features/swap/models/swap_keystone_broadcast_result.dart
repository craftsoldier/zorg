import 'swap_deposit_broadcast_result.dart';

class SwapKeystoneBroadcastResult extends SwapDepositBroadcastResult {
  const SwapKeystoneBroadcastResult({
    required super.txHash,
    required super.status,
    super.message,
  });
}
