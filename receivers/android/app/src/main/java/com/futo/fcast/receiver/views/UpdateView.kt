package com.futo.fcast.receiver.views

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import com.futo.fcast.receiver.BuildConfig
import com.futo.fcast.receiver.R
import com.futo.fcast.receiver.composables.ThemedText
import com.futo.fcast.receiver.composables.colorButtonPrimary
import com.futo.fcast.receiver.composables.colorButtonSecondary
import com.futo.fcast.receiver.models.MainActivityViewModel
import com.futo.fcast.receiver.models.UpdateState

@Composable
fun ProgressBarView(viewModel: MainActivityViewModel) {
    val animatedProgress by animateFloatAsState(targetValue = viewModel.updateProgress)

    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(40.dp)
            .padding(top = 10.dp)
            .clip(RoundedCornerShape(50.dp))
            .border(1.dp, Color(0xFF4E4E4E), RoundedCornerShape(50.dp))
            .background(
                Brush.verticalGradient(
                    colors = listOf(Color(0x80141414), Color(0x80505050))
                )
            )
    ) {
        Box(
            modifier = Modifier
                .fillMaxHeight()
                .fillMaxWidth(animatedProgress)
                .clip(RoundedCornerShape(50.dp))
                .background(
                    Brush.verticalGradient(
                        colors = listOf(Color(0xFF008BD7), Color(0xFF0069AA))
                    )
                )
        )
    }
}

@Composable
fun UpdateView(viewModel: MainActivityViewModel, modifier: Modifier = Modifier) {
    @Suppress("SENSELESS_COMPARISON")
    if (!BuildConfig.IS_PLAYSTORE_VERSION) {
        if (viewModel.updateState != UpdateState.NoUpdateAvailable) {
            val focusRequester = remember { FocusRequester() }
            if (viewModel.updateState == UpdateState.UpdateAvailable) {
                LaunchedEffect(Unit) {
                    focusRequester.requestFocus()
                }
            }

            Column(
                modifier = modifier,
                horizontalAlignment = Alignment.CenterHorizontally
            ) {
                val cardTitle = when (viewModel.updateState) {
                    UpdateState.InstallSuccess -> R.string.update_success
                    UpdateState.InstallFailure -> R.string.update_error
                    else -> R.string.update_available
                }

                ThemedText(
                    stringResource(cardTitle),
                    Modifier.padding(top = 10.dp),
                    fontWeight = FontWeight.Bold
                )
                Spacer(
                    modifier = Modifier
                        .padding(vertical = 10.dp)
                        .height(1.dp)
                        .fillMaxWidth()
                        .background(Color.Gray)
                )

                if (viewModel.updateState == UpdateState.InstallSuccess || viewModel.updateState == UpdateState.InstallFailure) {
                    Image(
                        painter = if (viewModel.updateState == UpdateState.InstallSuccess)
                            painterResource(R.drawable.ic_checked)
                        else painterResource(R.drawable.ic_error),
                        contentDescription = null,
                        modifier = Modifier.size(40.dp)
                    )
                    Spacer(
                        modifier = Modifier
                            .fillMaxWidth()
                            .height(10.dp)
                    )
                }
                ThemedText(viewModel.updateStatus, Modifier.padding(bottom = 10.dp))

                when (viewModel.updateState) {
                    UpdateState.UpdateAvailable -> {
                        Row(
                            modifier = Modifier,
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            Button(
                                onClick = viewModel::update,
                                modifier = Modifier.focusRequester(focusRequester),
                                enabled = true,
                                colors = ButtonDefaults.buttonColors(
                                    containerColor = colorButtonPrimary
                                )
                            ) {
                                ThemedText(stringResource(R.string.update))
                            }
                            Spacer(modifier = Modifier.width(10.dp))
                            Button(
                                onClick = { viewModel.updateState = UpdateState.NoUpdateAvailable },
                                modifier = Modifier,
                                enabled = true,
                                colors = ButtonDefaults.buttonColors(
                                    containerColor = colorButtonSecondary
                                )
                            ) {
                                ThemedText(stringResource(R.string.update_later))
                            }
                        }
                    }

                    UpdateState.Downloading -> {
                        Row(
                            horizontalArrangement = Arrangement.Center,
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            ProgressBarView(viewModel)
                        }
                    }

                    UpdateState.Installing -> {
                        CircularProgressIndicator(
                            modifier = Modifier.size(40.dp),
                            color = Color.White,
                            strokeWidth = 4.dp
                        )
                    }

                    else -> {}
                }
            }
        }
    }
}

@Preview
@Composable
fun UpdateViewPreview() {
    val viewModel = MainActivityViewModel()
    viewModel.updateState = UpdateState.UpdateAvailable
    viewModel.updateStatus = stringResource(R.string.update_status)
    UpdateView(viewModel)
}

@Preview
@Composable
fun UpdateViewPreviewUpdating() {
    val viewModel = MainActivityViewModel()
    viewModel.updateState = UpdateState.Downloading
    viewModel.updateStatus = stringResource(R.string.downloading_update)
    viewModel.updateProgress = 0.5f
    UpdateView(viewModel)
}

@Preview
@Composable
fun UpdateViewPreviewUpdateInstalling() {
    val viewModel = MainActivityViewModel()
    viewModel.updateState = UpdateState.Installing
    viewModel.updateStatus = stringResource(R.string.installing_update)
    UpdateView(viewModel)
}

@Preview
@Composable
fun UpdateViewPreviewUpdateSuccess() {
    val viewModel = MainActivityViewModel()
    viewModel.updateState = UpdateState.InstallSuccess
    viewModel.updateStatus = stringResource(R.string.success)
    UpdateView(viewModel)
}

@Preview
@Composable
fun UpdateViewPreviewUpdateFail() {
    val viewModel = MainActivityViewModel()
    viewModel.updateState = UpdateState.InstallFailure
    viewModel.updateStatus = "Error text"
    UpdateView(viewModel)
}
