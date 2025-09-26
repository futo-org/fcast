package com.futo.fcast.receiver

import androidx.media3.common.C
import androidx.media3.common.util.UnstableApi
import androidx.media3.exoplayer.source.preload.DefaultPreloadManager
import androidx.media3.exoplayer.source.preload.TargetPreloadStatusControl
import com.futo.fcast.receiver.models.PlaylistContent


@UnstableApi
class MediaPreloadStatusControl(val _playlistContent: PlaylistContent) :
    TargetPreloadStatusControl<Int, DefaultPreloadManager.PreloadStatus> {
    var currentItemIndex: Int = C.INDEX_UNSET


    override fun getTargetPreloadStatus(index: Int): DefaultPreloadManager.PreloadStatus? {
//        Log.i("preload", "preload index: $index")

        if (index < 0 || index >= _playlistContent.items.size) {
            return null
        }
        if (_playlistContent.items[index].cache != true) {
            return null
        }

        val forwardCacheAmount = _playlistContent.forwardCache ?: 0
        val isForwardCacheCandidate = (index > currentItemIndex) &&
                (index <= currentItemIndex + forwardCacheAmount)

        val backwardCacheAmount = _playlistContent.backwardCache ?: 0
        val isBackwardCacheCandidate = (index < currentItemIndex) &&
                (index >= currentItemIndex - backwardCacheAmount)

        return if (isForwardCacheCandidate || isBackwardCacheCandidate) {
            DefaultPreloadManager.PreloadStatus.specifiedRangeLoaded(
                _playlistContent.items[index].time?.toLong()?.times(1000) ?: 0,
                5_000
            )
        } else {
            null
        }
    }
}
