package com.futo.fcast.receiver.views

import androidx.annotation.OptIn
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.blur
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.media3.common.MediaMetadata.MEDIA_TYPE_VIDEO
import androidx.media3.common.util.Log
import androidx.media3.common.util.UnstableApi
import com.futo.fcast.receiver.PlayerActivity
import com.futo.fcast.receiver.R
import com.futo.fcast.receiver.composables.PlayerState
import com.futo.fcast.receiver.composables.ThemedText
import com.futo.fcast.receiver.composables.colorButtonSecondary
import com.futo.fcast.receiver.composables.colorCardBackground
import com.futo.fcast.receiver.composables.noRippleClickable
import com.futo.fcast.receiver.composables.strokeCardBorder
import com.futo.fcast.receiver.models.PlayerActivityViewModel
import com.futo.fcast.receiver.models.SettingsDialogMenuType
import com.futo.fcast.receiver.models.playbackSpeeds

private const val TAG = "PlayerSettingsView"

@Composable
fun SettingsDialogItem(
    playerState: PlayerState,
    iconId: Int?,
    titleText: String,
    valueText: String?,
    focus: Boolean,
    onClick: () -> Unit,
) {
    var selected by remember { mutableStateOf(false) }
    selected =
        titleText == playerState.selectedSubtitles || titleText == playerState.selectedPlaybackSpeed

    Row(
        modifier = Modifier
            .fillMaxWidth()
            .height(35.dp)
            .clip(RoundedCornerShape(12))
            .then(if (focus) Modifier.background(colorButtonSecondary) else Modifier)
            .noRippleClickable(onClick),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceEvenly
    ) {
        Row(
            modifier = Modifier
                .padding(6.dp)
        ) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.Start
            ) {
                if (iconId != null) {
                    Image(
                        painter = painterResource(iconId),
                        contentDescription = null,
                        modifier = Modifier
                            .size(16.dp)
                    )
                    Spacer(modifier = Modifier.size(10.dp))
                }

                ThemedText(
                    titleText,
                    fontSize = 12.sp,
                    fontWeight = FontWeight.Normal,
                    textAlign = TextAlign.Start
                )
            }

            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.End
            ) {
                if (valueText != null) {
                    Spacer(modifier = Modifier.size(10.dp))
                    ThemedText(
                        valueText,
                        modifier = Modifier.weight(1f),
                        fontSize = 12.sp,
                        fontWeight = FontWeight.Normal,
                        textAlign = TextAlign.End
                    )
                    Spacer(modifier = Modifier.size(2.dp))
                }

                Image(
                    painter = if (selected) painterResource(R.drawable.ic_check) else painterResource(
                        R.drawable.ic_arrow_right
                    ),
                    contentDescription = null,
                    modifier = Modifier.size(16.dp)
                )
            }
        }
    }
}

@Composable
fun DialogCard(
    modifier: Modifier = Modifier,
    content: @Composable (ColumnScope.() -> Unit)
) {
    Box(
        modifier = modifier
    ) {
        Card(
            modifier = Modifier
                .matchParentSize()
                .clip(RoundedCornerShape(16.dp))
                .blur(10.dp),
            colors = CardDefaults.cardColors(containerColor = colorCardBackground),
            border = strokeCardBorder,
            elevation = CardDefaults.cardElevation(defaultElevation = 8.dp)
        ) {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .background(colorCardBackground)
            ) { }
        }
        // DropdownMenu has issues with styling exactly as intended
        Card(
            modifier = Modifier,
            colors = CardDefaults.cardColors(containerColor = colorCardBackground),
            border = strokeCardBorder,
            elevation = CardDefaults.cardElevation(defaultElevation = 8.dp)
        ) {
            Column(
                modifier = Modifier.padding(10.dp),
                horizontalAlignment = Alignment.CenterHorizontally
            ) {
                content()
            }
        }
    }
}


@OptIn(UnstableApi::class)
@Composable
fun ListItemSettingsDialog(
    viewModel: PlayerActivityViewModel,
    modifier: Modifier = Modifier,
    playerState: PlayerState,
    dialogType: SettingsDialogMenuType,
    title: String,
    items: List<String>,
    titleOnClick: () -> Unit,
) {
    DialogCard(modifier) {
        var titleSelected by remember { mutableStateOf(false) }
        titleSelected =
            viewModel.settingsControlFocus.first == dialogType && viewModel.settingsControlFocus.second == 1

        Row(
            modifier = Modifier
                .fillMaxWidth()
                .height(35.dp)
                .clip(RoundedCornerShape(12))
                .then(if (titleSelected) Modifier.background(colorButtonSecondary) else Modifier)
                .noRippleClickable(titleOnClick),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.SpaceEvenly
        ) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.Start,
                modifier = Modifier
                    .fillMaxWidth()
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.Start
                ) {
                    Image(
                        painter = painterResource(R.drawable.ic_arrow_left),
                        contentDescription = null,
                        modifier = Modifier
                            .size(24.dp)
                    )
                    ThemedText(
                        title,
                        fontSize = 14.sp,
                        fontWeight = FontWeight.SemiBold
                    )
                }
            }
        }

        HorizontalDivider(modifier = Modifier.padding(horizontal = 0.dp, vertical = 8.dp))

        val lazyListState = rememberLazyListState()
        LazyColumn(
            state = lazyListState
        ) {
            itemsIndexed(items) { index, item ->
                val focus =
                    viewModel.settingsControlFocus.first == dialogType && viewModel.settingsControlFocus.second == index + 2

                if (focus) {
                    val isNextItemVisible by remember {
                        derivedStateOf {
                            lazyListState.layoutInfo.visibleItemsInfo.any {
                                if (index == items.size - 1) true else it.index == index + 1
                            }
                        }
                    }
                    val isPreviousItemVisible by remember {
                        derivedStateOf {
                            lazyListState.layoutInfo.visibleItemsInfo.any {
                                if (index == 0) true else it.index == index - 1
                            }
                        }
                    }

                    LaunchedEffect(isNextItemVisible) {
                        if (!isNextItemVisible && index < items.size) {
                            Log.i(TAG, "Scrolling to item ${index + 1}")
                            lazyListState.scrollToItem(index + 1)
                        }
                    }
                    LaunchedEffect(isPreviousItemVisible) {
                        if (!isPreviousItemVisible && index > 0) {
                            Log.i(TAG, "Scrolling to item ${index - 1}")
                            lazyListState.scrollToItem(index - 1)
                        }
                    }
                }

                SettingsDialogItem(
                    playerState,
                    null,
                    item,
                    null,
                    focus,
                ) {
                    if (dialogType == SettingsDialogMenuType.Subtitles) {
                        PlayerActivity.instance?.updateSubtitles(item)
                    } else if (dialogType == SettingsDialogMenuType.PlaybackSpeed) {
                        PlayerActivity.instance?.updatePlaybackSpeed(item.toFloat())
                    }
                }
            }
        }
    }
}

@Composable
fun SettingsDialog(
    viewModel: PlayerActivityViewModel,
    modifier: Modifier = Modifier,
    playerState: PlayerState,
) {
    DialogCard(modifier) {
        ThemedText(
            stringResource(R.string.playback_settings),
            fontSize = 14.sp,
            fontWeight = FontWeight.SemiBold
        )
        HorizontalDivider(modifier = Modifier.padding(horizontal = 0.dp, vertical = 8.dp))

        if (playerState.mediaType == MEDIA_TYPE_VIDEO) {
            SettingsDialogItem(
                playerState,
                iconId = R.drawable.ic_cc_on,
                titleText = stringResource(R.string.captions),
                valueText = playerState.selectedSubtitles,
                focus = viewModel.settingsControlFocus.first == SettingsDialogMenuType.Settings && viewModel.settingsControlFocus.second == 1,
            ) {
                viewModel.showSubtitlesSettingsDialog()
            }
        }

        SettingsDialogItem(
            playerState,
            iconId = R.drawable.ic_playback_speed,
            titleText = stringResource(R.string.playback_speed),
            valueText = playerState.selectedPlaybackSpeed,
            focus = viewModel.settingsControlFocus.first == SettingsDialogMenuType.Settings && viewModel.settingsControlFocus.second == 2,
        ) {
            viewModel.showPlaybackSpeedSettingsDialog()
        }
    }
}

@Preview
@Composable
fun SettingsDialogPreview() {
    val viewModel = PlayerActivityViewModel()
    val playerState = PlayerState(
        mediaType = MEDIA_TYPE_VIDEO
    )
    viewModel.settingsControlFocus = Pair(SettingsDialogMenuType.Settings, 0)

    SettingsDialog(
        viewModel,
        modifier = Modifier
            .width(250.dp)
            .padding(16.dp),
        playerState
    )
}

@Preview
@Composable
fun SettingsDialogSubtitlesPreview() {
    val viewModel = PlayerActivityViewModel()
    val playerState = PlayerState()
    val subtitles = listOf("Off", "English", "Spanish", "French", "German")

    ListItemSettingsDialog(
        viewModel,
        modifier = Modifier
            .width(250.dp)
            .padding(16.dp),
        playerState,
        SettingsDialogMenuType.Subtitles,
        stringResource(R.string.captions),
        subtitles,
    ) {}
}

@Preview
@Composable
fun SettingsDialogPlaybackSpeedPreview() {
    val viewModel = PlayerActivityViewModel()
    val playerState = PlayerState()
    viewModel.settingsControlFocus = Pair(SettingsDialogMenuType.PlaybackSpeed, 0)

    ListItemSettingsDialog(
        viewModel,
        modifier = Modifier
            .width(250.dp)
            .padding(16.dp),
        playerState,
        SettingsDialogMenuType.PlaybackSpeed,
        stringResource(R.string.playback_speed),
        playbackSpeeds,
    ) {}
}
