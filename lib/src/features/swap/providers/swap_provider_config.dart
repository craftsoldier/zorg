import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../integrations/near_intents/near_intents_one_click_swap_adapter.dart';
import '../models/swap_models.dart';

final _oneClickBaseUri = Uri.parse(
  'https://functions.vizor.cash/api/near-intents/1click',
);

final swapIntentProvider = Provider<SwapProvider>((ref) {
  return NearIntentsOneClickSwapAdapter(
    baseUri: _oneClickBaseUri,
    referral: 'vizor',
  );
});

final swapStatusPollIntervalProvider = Provider<Duration>((ref) {
  return const Duration(seconds: 20);
});

final swapPriceRefreshIntervalProvider = Provider<Duration>((ref) {
  return const Duration(seconds: 30);
});
