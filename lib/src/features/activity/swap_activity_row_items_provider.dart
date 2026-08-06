import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../swap/models/swap_intent_presentation_mapper.dart';
import '../swap/providers/swap_activity_store.dart';
import 'swap_activity_row_mapper.dart';

final swapActivityRowItemsProvider =
    FutureProvider.family<List<SwapActivityRowItem>, String>((
      ref,
      accountUuid,
    ) async {
      final records = await ref.watch(
        swapActivityRecordsProvider(accountUuid).future,
      );
      // Resolve the deadline-derived display status so the activity list agrees
      // with the detail panel (which resolves via swapIntentsFromRecords).
      final resolved = [
        for (final record in records) resolveSwapRecordForDisplay(record),
      ];
      return swapActivityRowItemsFromRecords(resolved);
    });
